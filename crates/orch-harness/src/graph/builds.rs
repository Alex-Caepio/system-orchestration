use super::*;

impl Graphs {
    pub(super) fn resolve_backend(&self, fresh: bool) -> Option<BuildBackend> {
        if self.force_native || Self::force_native_backend() {
            return Some(BuildBackend::NativeIr);
        }
        if self.program.is_some() {
            return self.tool(fresh).map(BuildBackend::Graphify);
        }
        Some(match self.tool(fresh) {
            Some(tool) => BuildBackend::Graphify(tool),
            None => BuildBackend::NativeIr,
        })
    }
    pub(crate) fn ir_store(&self, project: &str) -> Result<Arc<crate::graph_ir::GraphIrStore>> {
        if let Some(existing) = lock(&self.ir_stores)?.get(project).cloned() {
            return Ok(existing);
        }
        let opened = Arc::new(crate::graph_ir::GraphIrStore::open(
            &self.store.state,
            project,
        )?);
        let mut stores = lock(&self.ir_stores)?;
        Ok(stores
            .entry(project.to_owned())
            .or_insert_with(|| opened)
            .clone())
    }
    /// Returns a cached `GraphIrStore` from `ir_stores` / `ir_stores_ro` if already open, or opens an
    /// existing store strictly read-only without creating files or writing `ir_meta`/DDL.
    pub(crate) fn get_ir_store_readonly(
        &self,
        project: &str,
    ) -> Option<Arc<crate::graph_ir::GraphIrStore>> {
        if let Ok(stores) = self.ir_stores.lock()
            && let Some(existing) = stores.get(project)
        {
            return Some(existing.clone());
        }
        if let Ok(stores_ro) = self.ir_stores_ro.lock()
            && let Some(existing) = stores_ro.get(project)
            && existing.db_path().is_file()
        {
            return Some(existing.clone());
        }
        let opened = Arc::new(
            crate::graph_ir::GraphIrStore::open_readonly(&self.store.state, project)
                .ok()
                .flatten()?,
        );
        if let Ok(mut stores_ro) = self.ir_stores_ro.lock() {
            return Some(
                stores_ro
                    .entry(project.to_owned())
                    .or_insert_with(|| opened)
                    .clone(),
            );
        }
        Some(opened)
    }
    pub(super) fn evict_ir_store(&self, project: &str) {
        if let Ok(mut stores) = self.ir_stores.lock() {
            stores.remove(project);
        }
        if let Ok(mut stores_ro) = self.ir_stores_ro.lock() {
            stores_ro.remove(project);
        }
        crate::graph_ir::GraphIrStore::evict_readonly_cache_for_dir(&folder(
            &self.store.state,
            project,
        ));
    }
    pub(super) fn stop_watcher(&self, project: &str) {
        let removed = self
            .watchers
            .lock()
            .ok()
            .and_then(|mut map| map.remove(project));
        if let Some(mut watcher) = removed {
            watcher.shutdown();
        }
    }
    pub(super) fn ensure_watcher(&self, project: &str, root: &Path) {
        if self.stopping.load(Ordering::Acquire) || !root.is_dir() {
            return;
        }
        {
            let Ok(watchers) = self.watchers.lock() else {
                return;
            };
            if watchers
                .get(project)
                .is_some_and(|existing| existing.root == root)
            {
                return;
            }
        }
        self.stop_watcher(project);
        let _ = self.me.upgrade();
    }
    /// Triggers incremental SQLite IR updates (and refreshes `live/graph.json` when using native IR)
    /// for modified/deleted workspace relative paths. Even when the `GraphDomain` UI view is
    /// disabled, forwards path updates through the Graph-IR invalidation bus so SkillCache,
    /// SystemGraph, and SDD remain coherent.
    pub fn record_live_paths(&self, project: &str, rel_paths: &[String]) {
        if self.stopping.load(Ordering::Acquire) || project.is_empty() || rel_paths.is_empty() {
            return;
        }
        let Ok(root) = self.root(project) else {
            return;
        };
        if !root.is_dir() {
            return;
        }
        let mut unique = BTreeSet::new();
        for raw in rel_paths {
            if let Some(rel) = crate::graph_ir::normalize_rel_path(&root, raw) {
                unique.insert(rel);
            }
        }
        if unique.is_empty() {
            return;
        }
        let paths: Vec<String> = unique.into_iter().collect();
        let _ = self.apply_ir_path_updates(project, &root, &paths);
    }
    fn emit_architecture_updated(&self, project: &str, setting: &Value) -> u64 {
        let rev = self.bump_ir_revision(project);
        self.maps.rebind_live_ir_revision(project, rev);
        let nodes = setting["built"]["nodes"].as_u64();
        let edges = setting["built"]["edges"].as_u64();
        self.emit_status_event(
            project,
            "ready",
            "architecture_updated",
            rev,
            nodes,
            edges,
            Some("Architecture rules updated.".to_owned()),
        );
        rev
    }


    fn emit_ir_invalidation_bus(
        &self,
        project: &str,
        root: &Path,
        delta: &crate::graph_ir::IrUpdateDelta,
    ) {
        let _ =
            crate::system_graph::SystemGraphDomain::invalidate_summary_cache_and_notify_for_ir_delta(
                &self.store,
                project,
                root,
                delta,
            );

        let _ = crate::sdd::on_ir_paths_updated(&self.store, project, root, delta);
        let _ = crate::results::on_ir_paths_updated(&self.store, project, root, delta);
        let _ = crate::clash::on_ir_paths_updated(&self.store, project, root, delta);
        let _ = crate::services::on_ir_paths_updated(&self.store, project, root, delta);
    }

    fn enrich_bus_delta(
        ir: Option<&crate::graph_ir::GraphIrStore>,
        rel_paths: &[String],
        delta: Option<crate::graph_ir::IrUpdateDelta>,
        rev: u64,
    ) -> crate::graph_ir::IrUpdateDelta {
        let mut bus_delta = delta.unwrap_or_else(|| crate::graph_ir::IrUpdateDelta {
            changed_paths: rel_paths.to_vec(),
            manifest_changed: rel_paths
                .iter()
                .any(|p| crate::graph_ast::is_graph_manifest_path(Path::new(p))),
            ..Default::default()
        });
        if !rel_paths.is_empty() {
            let mut merged: BTreeSet<String> = bus_delta.changed_paths.into_iter().collect();
            merged.extend(rel_paths.iter().cloned());
            bus_delta.changed_paths = merged.into_iter().collect();
            bus_delta.manifest_changed |= rel_paths
                .iter()
                .any(|p| crate::graph_ast::is_graph_manifest_path(Path::new(p)));
        }
        if let Some(ir) = ir {
            let caller_targets: Vec<String> =
                if bus_delta.edited_files.is_empty() && bus_delta.removed_files.is_empty() {
                    bus_delta.changed_paths.clone()
                } else {
                    bus_delta
                        .edited_files
                        .iter()
                        .chain(bus_delta.removed_files.iter())
                        .cloned()
                        .collect()
                };
            bus_delta.incoming_caller_files = crate::graph_impact::resolve_incoming_caller_files(
                ir,
                &caller_targets,
                &bus_delta.incoming_caller_files,
            );
        }
        bus_delta.ir_revision = rev;
        bus_delta
    }

    pub(super) fn finalize_ir_update_delta(
        &self,
        project: &str,
        root: &Path,
        rel_paths: &[String],
    ) -> Result<Option<crate::graph_ir::IrUpdateDelta>> {
        let plock = self.project_publish_lock(project);
        let _publish_guard = lock(&plock)?;
        let _guard = lock(&self.ir_write)?;
        if self.stopping.load(Ordering::Acquire) {
            return Ok(None);
        }
        let setting = self.setting(project)?;
        let arch_only = rel_paths
            .iter()
            .all(|p| crate::graph_ast::is_graph_architecture_path(Path::new(p)));
        if setting["enabled"] != true {
            let rev = self.bump_ir_revision(project);
            if arch_only {
                return Ok(Some(Self::enrich_bus_delta(None, rel_paths, None, rev)));
            }
            if crate::graph_ir::GraphIrStore::exists(&self.store.state, project) {
                let ir = self.ir_store(project)?;
                let delta =
                    if ir.summary_counts()?.is_none() || rel_paths.len() >= MAX_INCREMENTAL_BATCH {
                        Some(ir.sync_workspace_with_delta(root)?)
                    } else {
                        Some(ir.update_paths_with_delta(root, rel_paths)?)
                    };
                return Ok(Some(Self::enrich_bus_delta(
                    Some(&ir),
                    rel_paths,
                    delta,
                    rev,
                )));
            }
            return Ok(Some(Self::enrich_bus_delta(None, rel_paths, None, rev)));
        }
        if arch_only {
            let rev = self.emit_architecture_updated(project, &setting);
            return Ok(Some(crate::graph_ir::IrUpdateDelta {
                changed_paths: rel_paths.to_vec(),
                ir_revision: rev,
                ..Default::default()
            }));
        }
        if let Some(job) = lock(&self.jobs)?.get(project) {
            if let Ok(mut pending) = job.pending_paths.lock() {
                for path in rel_paths {
                    if pending.len() >= MAX_WATCHER_COLLECT {
                        break;
                    }
                    if !pending.contains(path) {
                        pending.push(path.clone());
                    }
                }
            }
            drop(setting);
            drop(_guard);
            drop(_publish_guard);
            let rev = self.ir_revision_of(project);
            let pending_delta = Self::enrich_bus_delta(None, rel_paths, None, rev);
            self.emit_ir_invalidation_bus(project, root, &pending_delta);
            return Ok(None);
        }
        let any_arch = rel_paths
            .iter()
            .any(|p| crate::graph_ast::is_graph_architecture_path(Path::new(p)));
        let started = Instant::now();
        let ir = self.ir_store(project)?;
        let (delta, is_sync_delta) =
            if ir.summary_counts()?.is_none() || rel_paths.len() >= MAX_INCREMENTAL_BATCH {
                (Some(ir.sync_workspace_with_delta(root)?), true)
            } else {
                (Some(ir.update_paths_with_delta(root, rel_paths)?), false)
            };
        if !is_sync_delta
            && delta.as_ref().is_some_and(|d| {
                d.edited_files.is_empty()
                    && d.removed_files.is_empty()
                    && d.re_resolved_files.is_empty()
                    && !d.manifest_changed
                    && !d.reexport_changed
            })
            && self.maps.latest_live_ir_index(project).is_some()
        {
            let mut d = delta.expect("checked Some above");
            d.ir_revision = if any_arch {
                self.emit_architecture_updated(project, &setting)
            } else {
                self.ir_revision_of(project)
            };
            return Ok(Some(d));
        }
        let backend = self.resolve_backend(false);
        let using_native = matches!(backend, Some(BuildBackend::NativeIr))
            || setting["built"]["tool"].as_str() == Some(crate::graph_ir::NATIVE_IR_TOOL);
        if !using_native {
            let rev = self.ir_revision_of(project);
            return Ok(Some(Self::enrich_bus_delta(
                Some(&ir),
                rel_paths,
                delta,
                rev,
            )));
        }
        if self.stopping.load(Ordering::Acquire) || self.setting(project)?["enabled"] != true {
            return Ok(None);
        }
        let home = folder(&self.store.state, project);
        let write_epoch = ir.current_write_epoch();
        let incremental_index = if !is_sync_delta
            && let Some(ref delta) = delta
            && let Some(prev_index) = self.maps.latest_live_ir_index(project)
            && let Some(ir_delta) = ir.export_ir_delta_for_files_with_root(Some(root), delta)?
        {
            let shared_delta = Arc::new(ir_delta.clone());
            prev_index
                .update_files(root, ir_delta)
                .map(|idx| (Arc::new(idx), shared_delta))
        } else {
            None
        };
        let (index, nodes, edges, pending_live_json) =
            if let Some((idx, shared_delta)) = incremental_index {
                let (nodes, edges) = idx.node_link_counts();
                (
                    idx,
                    nodes,
                    edges,
                    Some(PendingLiveJson::Deltas(vec![shared_delta])),
                )
            } else {
                let (ir_nodes, ir_links, nodes, edges) = ir.export_ir_graph_elements()?;
                let shared_nodes = Arc::new(ir_nodes);
                let shared_links = Arc::new(ir_links);
                let idx = Arc::new(crate::graph_map::Index::from_ir_parts(
                    root,
                    Some(ir.db_path().to_path_buf()),
                    Some(crate::graph_ir::NATIVE_IR_TOOL.to_owned()),
                    (*shared_nodes).clone(),
                    (*shared_links).clone(),
                )?);
                (
                    idx,
                    nodes,
                    edges,
                    Some(PendingLiveJson::Elements {
                        nodes,
                        edges,
                        ir_nodes: shared_nodes,
                        ir_links: shared_links,
                    }),
                )
            };
        let rev = self.publish_native_index(project, write_epoch, index);
        let fresh_print = self.print(project, root, true);
        let elapsed = tenth(started.elapsed().as_secs_f64());
        let hash = fresh_print.as_ref().map(|p| p.hash.clone());
        let commit_owned = fresh_print.and_then(|p| p.commit);
        let bytes = setting["built"]["bytes"].as_u64().unwrap_or(0);
        let disk = size(&home);
        self.edit(project, |setting| {
            if setting["enabled"] != true || self.stopping.load(Ordering::Acquire) {
                return;
            }
            setting["built"] = json!({
                "at": store::now(),
                "seconds": elapsed,
                "tail": 0.0,
                "nodes": nodes,
                "edges": edges,
                "bytes": bytes,
                "disk": disk,
                "print": hash,
                "commit": commit_owned,
                "tool": crate::graph_ir::NATIVE_IR_TOOL,
                "program": Value::Null,
            });
            if let Some(fields) = setting.as_object_mut() {
                fields.remove("failure");
            }
        })?;
        self.schedule_live_json_write(project, root.to_path_buf(), pending_live_json);
        if let Some(ref d) = delta {
            self.emit_status_event_with_delta(
                project,
                "ready",
                "incremental_update",
                rev,
                Some(nodes),
                Some(edges),
                Some(NATIVE_READY_MESSAGE.to_owned()),
                d,
                is_sync_delta,
            );
        } else {
            self.emit_status_event(
                project,
                "ready",
                "incremental_update",
                rev,
                Some(nodes),
                Some(edges),
                Some(NATIVE_READY_MESSAGE.to_owned()),
            );
        }
        Ok(Some(Self::enrich_bus_delta(
            Some(&ir),
            rel_paths,
            delta,
            rev,
        )))
    }

    pub(super) fn apply_ir_path_updates(
        &self,
        project: &str,
        root: &Path,
        rel_paths: &[String],
    ) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) || rel_paths.is_empty() || !root.is_dir() {
            return Ok(());
        }
        if let Some(delta) = self.finalize_ir_update_delta(project, root, rel_paths)? {
            self.emit_ir_invalidation_bus(project, root, &delta);
        }
        Ok(())
    }
    pub(super) fn print(&self, project: &str, root: &Path, fresh: bool) -> Option<Print> {
        let mut prints = self.prints.lock().ok()?;
        if !fresh
            && let Some((at, found)) = prints.get(project)
            && at.elapsed() < PRINT_AGE
        {
            return found.clone();
        }
        let found = print(root);
        prints.insert(project.to_owned(), (Instant::now(), found.clone()));
        found
    }
    pub(super) fn busy(&self, project: &str) -> Result<bool> {
        Ok(self.store.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE project_id=?1 AND system_run IS NULL AND system_discovery IS NULL AND status IN('queued','running'))",
            [project],
            |row| row.get(0),
        )?)
    }
    pub(super) fn root(&self, project: &str) -> Result<PathBuf> {
        Ok(self.target_info(project)?.0)
    }
    /// Starts a build when the trigger warrants one. An automatic trigger
    /// builds only what changed since the last attempt, never within the gap
    /// after the previous build, and never under a run except at its launch.
    pub fn refresh(&self, project: &str, trigger: Trigger) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let setting = self.setting(project)?;
        if setting["enabled"] != true {
            return Ok(());
        }
        if let Some(job) = lock(&self.jobs)?.get(project) {
            if trigger == Trigger::Finish {
                job.again.store(true, Ordering::Release);
            }
            return Ok(());
        }
        let asked = matches!(trigger, Trigger::Enabled | Trigger::Manual);
        // One run of the tool at a time unless the person asks for another.
        if !asked && lock(&self.revisions)?.contains_key(project) {
            return Ok(());
        }
        let Some(backend) = self.resolve_backend(asked) else {
            return Ok(());
        };
        let root = self.root(project)?;
        if matches!(backend, BuildBackend::NativeIr) {
            self.ensure_watcher(project, &root);
        }
        let print = self.print(project, &root, trigger != Trigger::Visit);
        if trigger != Trigger::Manual {
            let now = print.as_ref().map(|print| print.hash.as_str());
            let published =
                setting["built"].is_object() && live(&self.store.state, project).is_file();
            let ir_missing = matches!(backend, BuildBackend::NativeIr)
                && !crate::graph_ir::GraphIrStore::exists(&self.store.state, project);
            if published
                && !ir_missing
                && (now.is_none() || setting["built"]["print"].as_str() == now)
            {
                return Ok(());
            }
            if setting["failure"].is_object() && setting["failure"]["print"].as_str() == now {
                return Ok(());
            }
        }
        if !asked {
            if trigger != Trigger::Finish
                && lock(&self.finished)?
                    .get(project)
                    .is_some_and(|at| at.elapsed() < self.gap)
            {
                return Ok(());
            }
            if trigger != Trigger::Launch && self.busy(project)? {
                return Ok(());
            }
        }
        self.begin(project, root, backend, print)
    }
    pub(super) fn begin(
        &self,
        project: &str,
        root: PathBuf,
        backend: BuildBackend,
        print: Option<Print>,
    ) -> Result<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(me) = self.me.upgrade() else {
            return Ok(());
        };
        let job = Arc::new(Job::new());
        {
            let mut jobs = lock(&self.jobs)?;
            if jobs.contains_key(project) {
                return Ok(());
            }
            jobs.insert(project.to_owned(), job.clone());
        }
        let rev = self.ir_revision_of(project);
        self.emit_status_event(project, "building", "build_started", rev, None, None, None);
        let name = project.to_owned();
        let worker = std::thread::Builder::new()
            .name("harness-graph".into())
            .spawn(move || {
                let commit = print.as_ref().and_then(|p| p.commit.clone());
                let outcome = match &backend {
                    BuildBackend::Graphify(tool) => {
                        me.build(&name, &root, tool, &job, commit.as_deref())
                    }
                    BuildBackend::NativeIr => {
                        me.build_native(&name, &root, &job, commit.as_deref())
                    }
                };
                me.settle(&name, &job, outcome, print, &backend);
            });
        match worker {
            Ok(handle) => {
                let mut workers = lock(&self.workers)?;
                workers.retain(|worker| !worker.is_finished());
                workers.push(handle);
                Ok(())
            }
            Err(_) => {
                lock(&self.jobs)?.remove(project);
                Err(HarnessError::unavailable(
                    "The code graph build could not start.",
                ))
            }
        }
    }
    /// Runs the tool over `root` with its output in `out` and waits for it.
    /// `Err` is how the build ended when no graph came of it.
    pub(super) fn run(
        &self,
        tool: &Tool,
        root: &Path,
        out: &Path,
        job: &Arc<Job>,
    ) -> std::result::Result<Ran, Outcome> {
        // A build that starts from the previous graph keeps nodes of files that
        // no longer exist; the cache beside it makes the clean build no slower.
        let fresh = out.join("graph.json");
        if let Err(error) = std::fs::remove_file(&fresh)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Outcome::Failed(
                "The previous build output could not be cleared.".into(),
            ));
        }
        let mut command = Command::new(&tool.path);
        command
            .arg("update")
            .arg(root)
            .current_dir(root)
            .env("GRAPHIFY_OUT", out)
            .env("GRAPHIFY_BACKEND", "none")
            .env("GRAPHIFY_NO_TIPS", "1")
            .env(NO_SKILL_REFRESH.0, NO_SKILL_REFRESH.1)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in PROVIDER_KEYS {
            command.env_remove(name);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let Ok(mut child) = command.spawn() else {
            return Err(Outcome::Failed("Graphify could not start.".into()));
        };
        let readers = [
            child
                .stdout
                .take()
                .and_then(|pipe| pump(pipe, job.clone(), false)),
            child
                .stderr
                .take()
                .and_then(|pipe| pump(pipe, job.clone(), true)),
        ];
        let stopped =
            || job.cancel.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire);
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code(),
                Ok(None) if stopped() => {
                    end(&mut child);
                    return Err(Outcome::Cancelled);
                }
                Ok(None) if job.started().elapsed() > BUILD_LIMIT => {
                    end(&mut child);
                    return Err(Outcome::Failed(
                        "Graphify did not finish within 30 minutes.".into(),
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => {
                    end(&mut child);
                    return Err(Outcome::Failed("Graphify could not be watched.".into()));
                }
            }
        };
        // Workers can outlive the leader and hold its pipes open.
        end(&mut child);
        let waiting = Instant::now();
        for reader in readers.into_iter().flatten() {
            while !reader.is_finished() && waiting.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        let elapsed = job.started().elapsed().as_secs_f64();
        let (read, built, line) = match job.seen.lock() {
            Ok(seen) => (
                seen.observed.read,
                seen.built,
                if seen.complaint.is_empty() {
                    seen.line.clone()
                } else {
                    seen.complaint.clone()
                },
            ),
            Err(_) => (None, None, String::new()),
        };
        if code != Some(0) {
            return Err(Outcome::Failed(if line.is_empty() {
                "Graphify stopped without a message.".into()
            } else {
                line
            }));
        }
        let bytes = match std::fs::metadata(&fresh) {
            Ok(meta) if meta.is_file() && meta.len() > 0 => meta.len(),
            _ => {
                return Err(Outcome::Failed(
                    "Graphify finished without writing a graph.".into(),
                ));
            }
        };
        if stopped() {
            return Err(Outcome::Cancelled);
        }
        Ok(Ran {
            fresh,
            elapsed,
            read,
            built,
            bytes,
        })
    }
    pub(super) fn build(
        &self,
        project: &str,
        root: &Path,
        tool: &Tool,
        job: &Arc<Job>,
        commit: Option<&str>,
    ) -> Outcome {
        let home = folder(&self.store.state, project);
        let work = home.join("work");
        let live = home.join("live");
        if std::fs::create_dir_all(&work).is_err()
            || std::fs::create_dir_all(&live).is_err()
            || orch_host::util::private_directory(&home).is_err()
        {
            return Outcome::Failed("The graph folder could not be prepared.".into());
        }
        let _kept = Kept::of(root);
        let ran = match self.run(tool, root, &work, job) {
            Ok(ran) => ran,
            Err(outcome) => return outcome,
        };
        let live_graph = live.join("graph.json");
        if std::fs::rename(&ran.fresh, &live_graph).is_err() {
            return Outcome::Failed("The new graph could not be published.".into());
        }
        write_live_summary(&live_graph, commit, None, ran.built);
        let prev_commit = self
            .setting(project)
            .ok()
            .and_then(|s| s["built"]["commit"].as_str().map(str::to_owned));
        let plock = self.project_publish_lock(project);
        let rev = if let Ok(_publish_guard) = plock.lock() {
            let rev = self.bump_ir_revision(project);
            self.maps.forget_live(project);
            rev
        } else {
            self.ir_revision_of(project)
        };
        let mut bus_delta = if crate::graph_ir::GraphIrStore::exists(&self.store.state, project)
            && let Ok(ir) = self.ir_store(project)
            && let Ok(mut delta) = ir.sync_workspace_with_delta(root)
        {
            delta.ir_revision = rev;
            delta
        } else {
            let mut changed = BTreeSet::new();
            if let Ok(work) = graph_changes::resolve(root, &Target::Work, &[]) {
                changed.extend(work.files.into_iter().map(|c| c.path));
            }
            if let (Some(base), Some(head)) = (prev_commit.as_deref(), commit)
                && base != head
                && let Ok(range) = graph_changes::resolve(
                    root,
                    &Target::Range {
                        from: base.to_owned(),
                        to: head.to_owned(),
                        three_dot: false,
                    },
                    &[],
                )
            {
                changed.extend(range.files.into_iter().map(|c| c.path));
            }
            let changed_paths: Vec<String> = changed.into_iter().collect();
            let manifest_changed = changed_paths
                .iter()
                .any(|p| crate::graph_ast::is_graph_manifest_path(Path::new(p)));
            crate::graph_ir::IrUpdateDelta {
                changed_paths,
                manifest_changed,
                ir_revision: rev,
                ..Default::default()
            }
        };
        if !bus_delta.changed_paths.is_empty()
            || !bus_delta.incoming_caller_files.is_empty()
            || bus_delta.manifest_changed
            || bus_delta.semantic_changed
            || bus_delta.reexport_changed
        {
            bus_delta.ir_revision = rev;
            self.emit_ir_invalidation_bus(project, root, &bus_delta);
        }
        Outcome::Built(ran.record(size(&home)))
    }
    pub(super) fn build_native(
        &self,
        project: &str,
        root: &Path,
        job: &Arc<Job>,
        commit: Option<&str>,
    ) -> Outcome {
        let stopped =
            || job.cancel.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire);
        if stopped() {
            return Outcome::Cancelled;
        }
        let home = folder(&self.store.state, project);
        let live_dir = home.join("live");
        if std::fs::create_dir_all(&live_dir).is_err()
            || orch_host::util::private_directory(&home).is_err()
        {
            return Outcome::Failed("The graph folder could not be prepared.".into());
        }
        let ir = match self.ir_store(project) {
            Ok(ir) => ir,
            Err(err) => return Outcome::Failed(err.message),
        };
        let plock = self.project_publish_lock(project);
        let publish_guard = match plock.lock() {
            Ok(guard) => guard,
            Err(_) => return Outcome::Failed("The code graph lock is poisoned.".into()),
        };
        let mut delta = match ir.sync_workspace_with_delta(root) {
            Ok(delta) => delta,
            Err(err) => return Outcome::Failed(err.message),
        };
        let stats = delta.stats.clone();
        if stopped() {
            return Outcome::Cancelled;
        }
        job.observe(
            &format!(
                "  AST extraction: {}/{} uncached files (100%)",
                stats.files_scanned, stats.files_scanned
            ),
            false,
        );
        let live_graph = live_dir.join("graph.json");
        let (ir_nodes, ir_links, nodes, edges) = match ir.export_ir_graph_elements() {
            Ok(parts) => parts,
            Err(err) => return Outcome::Failed(err.message),
        };
        if stopped() {
            return Outcome::Cancelled;
        }
        let write_epoch = ir.current_write_epoch();
        let shared_nodes = Arc::new(ir_nodes);
        let shared_links = Arc::new(ir_links);
        let index = match crate::graph_map::Index::from_ir_parts(
            root,
            Some(ir.db_path().to_path_buf()),
            Some(crate::graph_ir::NATIVE_IR_TOOL.to_owned()),
            (*shared_nodes).clone(),
            (*shared_links).clone(),
        ) {
            Ok(idx) => Arc::new(idx),
            Err(err) => return Outcome::Failed(err.message),
        };
        let can_defer_live_json = live_graph.is_file()
            && self
                .setting(project)
                .ok()
                .and_then(|s| s["built"]["tool"].as_str().map(str::to_owned))
                .as_deref()
                == Some(crate::graph_ir::NATIVE_IR_TOOL);
        let rev = if can_defer_live_json {
            let rev = self.publish_native_index(project, write_epoch, index);
            self.schedule_live_json_write(
                project,
                root.to_path_buf(),
                Some(PendingLiveJson::Elements {
                    nodes,
                    edges,
                    ir_nodes: shared_nodes,
                    ir_links: shared_links,
                }),
            );
            rev
        } else {
            if let Err(err) = crate::graph_ir::write_live_graph_from_elements_atomic(
                &live_graph,
                &shared_nodes,
                &shared_links,
            ) {
                return Outcome::Failed(err.message);
            }
            if stopped() {
                return Outcome::Cancelled;
            }
            write_live_summary(&live_graph, commit, Some((nodes, edges)), None);
            self.publish_native_index(project, write_epoch, index)
        };
        drop(publish_guard);
        if !delta.changed_paths.is_empty()
            || !delta.incoming_caller_files.is_empty()
            || delta.manifest_changed
            || delta.semantic_changed
            || delta.reexport_changed
        {
            delta.ir_revision = rev;
            self.emit_ir_invalidation_bus(project, root, &delta);
        }
        let bytes = std::fs::metadata(&live_graph).map_or(0, |m| m.len());
        let elapsed = job.started().elapsed().as_secs_f64();
        Outcome::Built(json!({
            "at": store::now(),
            "seconds": tenth(elapsed),
            "tail": 0.0,
            "nodes": nodes,
            "edges": edges,
            "bytes": bytes,
            "disk": size(&home),
        }))
    }
    pub(super) fn settle(
        &self,
        project: &str,
        job: &Job,
        outcome: Outcome,
        print: Option<Print>,
        backend: &BuildBackend,
    ) {
        let purge = job.purge.load(Ordering::Acquire);
        if purge {
            self.stop_watcher(project);
            self.cancel_live_json_writer(project);
            self.evict_ir_store(project);
            let _ = std::fs::remove_dir_all(folder(&self.store.state, project));
        }
        type PendingStatusEvent = (
            String,
            &'static str,
            Option<u64>,
            Option<u64>,
            Option<String>,
        );
        let hash = print.as_ref().map(|print| print.hash.clone());
        let mut event_payload: Option<PendingStatusEvent> = None;
        let _ = self.edit(project, |setting| {
            if purge {
                let target_root = setting.get("target_root").cloned();
                let target_name = setting.get("target_name").cloned();
                *setting = json!({"enabled": false, "disabled_by_user": true});
                if let Some(root) = target_root {
                    setting["target_root"] = root;
                }
                if let Some(name) = target_name {
                    setting["target_name"] = name;
                }
                event_payload = Some(("off".to_owned(), "deleted", None, None, None));
                return;
            }
            match outcome {
                Outcome::Built(mut built) => {
                    let nodes = built["nodes"].as_u64();
                    let edges = built["edges"].as_u64();
                    built["print"] = json!(hash);
                    built["commit"] = json!(print.as_ref().and_then(|print| print.commit.clone()));
                    match backend {
                        BuildBackend::Graphify(tool) => {
                            built["tool"] = json!(tool.version);
                            built["program"] = json!(tool.path.to_string_lossy());
                        }
                        BuildBackend::NativeIr => {
                            built["tool"] = json!(crate::graph_ir::NATIVE_IR_TOOL);
                            built["program"] = Value::Null;
                        }
                    }
                    setting["built"] = built;
                    if let Some(fields) = setting.as_object_mut() {
                        fields.remove("failure");
                    }
                    event_payload = Some((
                        "ready".to_owned(),
                        "build_finished",
                        nodes,
                        edges,
                        Some(NATIVE_READY_MESSAGE.to_owned()),
                    ));
                }
                Outcome::Failed(message) => {
                    event_payload = Some((
                        "failed".to_owned(),
                        "build_failed",
                        None,
                        None,
                        Some(message.clone()),
                    ));
                    setting["failure"] =
                        json!({"at": store::now(), "message": message, "print": hash});
                }
                Outcome::Cancelled => {}
            }
        });
        if let Some((state, reason, nodes, edges, message)) = event_payload {
            let rev = self.ir_revision_of(project);
            self.emit_status_event(project, &state, reason, rev, nodes, edges, message);
        }
        if let Ok(mut finished) = self.finished.lock() {
            finished.insert(project.to_owned(), Instant::now());
        }
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.remove(project);
        }
        let pending_paths = job
            .pending_paths
            .lock()
            .map(|mut p| std::mem::take(&mut *p))
            .unwrap_or_default();
        if !pending_paths.is_empty()
            && !purge
            && !self.stopping.load(Ordering::Acquire)
            && let Ok(root) = self.root(project)
        {
            let _ = self.apply_ir_path_updates(project, &root, &pending_paths);
        }
        if job.again.load(Ordering::Acquire) && !purge {
            if let Ok(mut finished) = self.finished.lock() {
                finished.remove(project);
            }
            let _ = self.refresh(project, Trigger::Finish);
        }
    }
}

