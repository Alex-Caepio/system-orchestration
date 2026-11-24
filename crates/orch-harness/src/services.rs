//! Required native domain ports; the application composes concrete Rust services.
use crate::{HarnessError, Result, store::SessionStore};
fn clean_relative_path_helper(raw: &str) -> Option<String> {
    let clean = crate::graph_live::clean_rel_key(raw);
    (!clean.is_empty()).then_some(clean)
}
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
pub enum Payload {
    Json(Value),
    Bytes {
        bytes: Vec<u8>,
        content_type: String,
        filename: Option<String>,
        etag: Option<String>,
        cache: Option<String>,
    },
}
pub type Reply<'a> = Pin<Box<dyn Future<Output = Result<Payload>> + Send + 'a>>;
pub type CloseFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
pub trait Services: Send + Sync {
    fn request<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a>;
    fn close<'a>(&'a self) -> CloseFuture<'a>;
    /// Subscribes to the live multiplexed SSE frame hub, replaying buffered
    /// frames after `last_event_id` when available.
    fn subscribe_stream(&self, _last_event_id: Option<u64>) -> Option<crate::store::ReplayStart> {
        None
    }
    /// Returns whether `graph_status` / `system_graph_status` SSE frames tagged
    /// with `event_pid` belong to a registered system related to `subscribed_pid`.
    fn allows_sse_graph_project(&self, _subscribed_pid: &str, _event_pid: &str) -> bool {
        false
    }
}

enum OmnigraphEnrichment {
    Unavailable,
    Ok { atoms: Value, warnings: Value },
    Skipped(Value),
    Error(String),
}

async fn fetch_omnigraph_enrichment(
    omnigraph: Option<Arc<dyn Domain>>,
    project_id: Value,
    path: Value,
) -> OmnigraphEnrichment {
    let Some(omnigraph) = omnigraph else {
        return OmnigraphEnrichment::Unavailable;
    };
    let file = json!({
        "params": {},
        "body": {},
        "query": {"project_id": project_id, "path": path},
    });
    match omnigraph.execute("omnigraph.file_context", file).await {
        Ok(Payload::Json(found)) if found["ok"] == true => OmnigraphEnrichment::Ok {
            warnings: json!(Vec::<Value>::new()),
            atoms: found["atoms"].clone(),
        },
        Ok(Payload::Json(found)) => OmnigraphEnrichment::Skipped(found["skipped"].clone()),
        Ok(_) => OmnigraphEnrichment::Unavailable,
        Err(error) => OmnigraphEnrichment::Error(error.message),
    }
}

fn apply_omnigraph_enrichment(links: &mut Value, enrichment: OmnigraphEnrichment) {
    // The shared graph's atoms are a companion: an unreachable Omnigraph
    // leaves the Brain and Bank links standing.
    links["omnigraph_atoms"] = json!([]);
    match enrichment {
        OmnigraphEnrichment::Unavailable => {}
        OmnigraphEnrichment::Ok { atoms, warnings } => {
            links["omnigraph_atoms"] = atoms;
            links["contradiction_warnings"] = warnings;
        }
        OmnigraphEnrichment::Skipped(skipped) => {
            links["omnigraph_skipped"] = skipped;
        }
        OmnigraphEnrichment::Error(err) => {
            links["omnigraph_error"] = json!(err);
        }
    }
}

fn collect_registered_system_members(
    store: &SessionStore,
) -> Vec<(String, std::collections::BTreeSet<String>)> {
    let Ok((by_root, allowed)) = crate::system_graph::registered_project_roots(store) else {
        return Vec::new();
    };
    if by_root.is_empty() {
        return Vec::new();
    }
    let extra_configs = crate::system_graph::extra_system_configs_by_project(store);

    let mut groups = Vec::new();
    for (proj_root, sys_pid) in &by_root {
        let mut candidates = vec![
            "system.json".to_owned(),
            "ai-system.json".to_owned(),
            ".harness/system.json".to_owned(),
        ];
        if let Some(extras) = extra_configs.get(sys_pid) {
            for cfg in extras {
                if !candidates.contains(cfg) {
                    candidates.push(cfg.clone());
                }
            }
        }
        let mut members = std::collections::BTreeSet::new();
        for rel in candidates {
            let cfg_path = proj_root.join(&rel);
            let Ok(meta) = std::fs::symlink_metadata(&cfg_path) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > 1024 * 1024 {
                continue;
            }
            let Ok(canon_cfg) = orch_core::fs::canonicalize(&cfg_path) else {
                continue;
            };
            if !allowed.iter().any(|root| canon_cfg.starts_with(root)) {
                continue;
            }
            let Ok(raw) = std::fs::read(&canon_cfg) else {
                continue;
            };
            let Ok(doc) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            let Some(services) = doc["services"].as_array() else {
                continue;
            };
            let cfg_parent = canon_cfg.parent().unwrap_or(proj_root.as_path());
            for entry in services {
                let Some(loc) = entry["root"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let Ok(canon_svc) = orch_core::fs::canonicalize(cfg_parent.join(loc)) else {
                    continue;
                };
                if !allowed.iter().any(|root| canon_svc.starts_with(root)) {
                    continue;
                }
                let (graph_id, _) =
                    crate::system_graph::compute_graph_id_for_root(&by_root, &canon_svc);
                members.insert(graph_id);
            }
        }
        if !members.is_empty() {
            members.insert(sys_pid.clone());
            groups.push((sys_pid.clone(), members));
        }
    }
    groups
}

fn related_system_projects_for(
    store: &SessionStore,
    subscribed_pid: &str,
) -> std::collections::BTreeSet<String> {
    let mut related = std::collections::BTreeSet::new();
    let pid = subscribed_pid.trim();
    if pid.is_empty() {
        return related;
    }
    for (sys_pid, members) in collect_registered_system_members(store) {
        if sys_pid == pid || members.contains(pid) {
            related.insert(sys_pid);
            related.extend(members);
        }
    }
    related
}

fn emit_system_graph_status_for_member_event(
    store: &SessionStore,
    ev: &crate::graph::GraphStatusEvent,
) {
    if ev.state == "building" {
        return;
    }
    let owning_systems: Vec<String> = collect_registered_system_members(store)
        .into_iter()
        .filter_map(|(sys_pid, members)| members.contains(&ev.project_id).then_some(sys_pid))
        .collect();
    if owning_systems.is_empty() {
        return;
    }
    for sys_pid in owning_systems {
        let payload = json!({
            "project_id": sys_pid,
            "member_project_id": ev.project_id,
            "state": ev.state,
            "reason": ev.reason,
            "revision": ev.revision,
            "ir_revision": ev.ir_revision,
        });
        store.publish_event("system_graph_status", None, Some(sys_pid), payload);
    }
}

fn ensure_graph_event_bridge(store: &SessionStore) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if !store.stream().claim_graph_bridge() {
        return;
    }
    let mut rx = crate::graph::subscribe_graph_events();
    let store = store.clone();
    handle.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if ev.state_dir == store.state {
                        let pid = ev.project_id.clone();
                        if let Ok(val) = serde_json::to_value(&ev) {
                            store.publish_event("graph_status", None, Some(pid), val);
                        }
                        emit_system_graph_status_for_member_event(&store, &ev);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    store.publish_event(
                        "resync",
                        None,
                        None,
                        json!({
                            "reason": "graph_bridge_lagged",
                            "skipped": skipped,
                        }),
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
pub trait Domain: Send + Sync {
    fn execute<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a>;

    /// Whether this domain emits authoritative `graph_status` events over the graph bridge.
    fn emits_graph_status_events(&self) -> bool {
        false
    }
}
const MAX_CONCURRENT_FILE_CONTEXT: usize = 8;
const MAX_PREFLIGHT_MEMO: usize = 128;
const MAX_SDD_ARTIFACTS_MEMO: usize = 64;
const MAX_KNOWLEDGE_SYNC_STAMPS: usize = 128;
const KNOWLEDGE_RACY_NS: i128 = 2_000_000_000;

fn ir_bus_epochs() -> &'static Mutex<HashMap<(PathBuf, String), u64>> {
    static EPOCHS: std::sync::OnceLock<Mutex<HashMap<(PathBuf, String), u64>>> =
        std::sync::OnceLock::new();
    EPOCHS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bumps the Graph-IR invalidation bus epoch for `project_id` so cached preflight
/// metadata invalidates even if `live/graph.json` or request epochs stayed unchanged.
pub(crate) fn on_ir_paths_updated(
    store: &SessionStore,
    project_id: &str,
    _root: &Path,
    _delta: &crate::graph_ir::IrUpdateDelta,
) -> u64 {
    let trimmed = project_id.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let Ok(mut guard) = ir_bus_epochs().lock() else {
        return 0;
    };
    let entry = guard
        .entry((store.state.clone(), trimmed.to_owned()))
        .or_insert(0);
    *entry = entry.saturating_add(1);
    *entry
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PreflightBaseKey {
    project_id: String,
    request_sha: String,
    ir_revision: u64,
    omnigraph_epoch: u64,
    knowledge_epoch: u64,
    graph_epoch: u64,
    ir_bus_epoch: u64,
    edition: String,
    knowledge_stamp: String,
    live_graph_stamp: String,
    workspace_paths_stamp: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PreflightSessionKey {
    session_id: String,
    session_epoch: u64,
    session_stamp: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PreflightMemoKey {
    base: PreflightBaseKey,
    session: Option<PreflightSessionKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SddArtifactsMemoKey {
    session_id: String,
    project_path: String,
    sdd_canonical: String,
    candidates_stamp: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct KnowledgeSyncStampKey {
    project_id: String,
    bank: String,
    session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KnowledgeDiskStamp {
    Absent,
    Racy,
    Settled(String),
}

#[derive(Default)]
struct ServicesRuntimeCache {
    seq: u64,
    omnigraph_epochs: HashMap<String, u64>,
    knowledge_epochs: HashMap<String, u64>,
    graph_epochs: HashMap<String, u64>,
    session_epochs: HashMap<String, u64>,
    preflight_memo: HashMap<PreflightMemoKey, (Value, u64)>,
    sdd_artifacts_memo: HashMap<SddArtifactsMemoKey, (Value, u64)>,
    #[cfg(test)]
    sdd_artifacts_hits: usize,
    knowledge_sync_stamps: HashMap<KnowledgeSyncStampKey, ((String, u64), u64)>,
}

impl ServicesRuntimeCache {
    fn epochs_for(&self, project_id: &str) -> (u64, u64, u64, u64) {
        (
            self.omnigraph_epochs.get(project_id).copied().unwrap_or(0),
            self.knowledge_epochs.get(project_id).copied().unwrap_or(0),
            self.graph_epochs.get(project_id).copied().unwrap_or(0),
            self.session_epochs.get(project_id).copied().unwrap_or(0),
        )
    }

    fn bump_omnigraph(&mut self, project_id: &str) {
        if !project_id.trim().is_empty() {
            *self
                .omnigraph_epochs
                .entry(project_id.trim().to_owned())
                .or_insert(0) += 1;
        }
    }

    fn bump_knowledge(&mut self, project_id: &str) {
        if !project_id.trim().is_empty() {
            *self
                .knowledge_epochs
                .entry(project_id.trim().to_owned())
                .or_insert(0) += 1;
        }
    }

    fn bump_graph(&mut self, project_id: &str) {
        if !project_id.trim().is_empty() {
            *self
                .graph_epochs
                .entry(project_id.trim().to_owned())
                .or_insert(0) += 1;
        }
    }

    fn bump_session(&mut self, project_id: &str) {
        if !project_id.trim().is_empty() {
            *self
                .session_epochs
                .entry(project_id.trim().to_owned())
                .or_insert(0) += 1;
        }
    }

    fn get_preflight(&mut self, key: &PreflightMemoKey) -> Option<Value> {
        self.seq = self.seq.saturating_add(1);
        let use_seq = self.seq;
        let entry = self.preflight_memo.get_mut(key)?;
        entry.1 = use_seq;
        Some(entry.0.clone())
    }

    fn insert_preflight(&mut self, key: PreflightMemoKey, value: Value) {
        self.seq = self.seq.saturating_add(1);
        let use_seq = self.seq;
        if self.preflight_memo.len() >= MAX_PREFLIGHT_MEMO
            && !self.preflight_memo.contains_key(&key)
            && let Some(evict) = self
                .preflight_memo
                .iter()
                .min_by_key(|(_, (_, s))| *s)
                .map(|(k, _)| k.clone())
        {
            self.preflight_memo.remove(&evict);
        }
        self.preflight_memo.insert(key, (value, use_seq));
    }

    fn get_sdd_artifacts(&mut self, key: &SddArtifactsMemoKey) -> Option<Value> {
        self.seq = self.seq.saturating_add(1);
        let use_seq = self.seq;
        let entry = self.sdd_artifacts_memo.get_mut(key)?;
        entry.1 = use_seq;
        #[cfg(test)]
        {
            self.sdd_artifacts_hits = self.sdd_artifacts_hits.saturating_add(1);
        }
        Some(entry.0.clone())
    }

    fn insert_sdd_artifacts(&mut self, key: SddArtifactsMemoKey, value: Value) {
        self.seq = self.seq.saturating_add(1);
        let use_seq = self.seq;
        if self.sdd_artifacts_memo.len() >= MAX_SDD_ARTIFACTS_MEMO
            && !self.sdd_artifacts_memo.contains_key(&key)
            && let Some(evict) = self
                .sdd_artifacts_memo
                .iter()
                .min_by_key(|(_, (_, s))| *s)
                .map(|(k, _)| k.clone())
        {
            self.sdd_artifacts_memo.remove(&evict);
        }
        self.sdd_artifacts_memo.insert(key, (value, use_seq));
    }
}

fn build_sdd_artifacts_memo_key(
    sid: &str,
    project_path: &str,
    sdd: &Value,
) -> Option<SddArtifactsMemoKey> {
    let root = Path::new(project_path);
    if project_path.trim().is_empty() || !root.is_dir() {
        return None;
    }
    let root_canon = orch_core::fs::canonicalize(root).ok()?;
    let feature = sdd["feature"].as_str().unwrap_or("");
    let mut candidates = vec![
        "specs/memory/constitution.md".to_owned(),
        "specs/memory/sdd-constitution.md".to_owned(),
    ];
    for name in ["spec.md", "plan.md", "tasks.md", "progress.md"] {
        candidates.push(format!("specs/{feature}/{name}"));
        candidates.push(format!("specs/{feature}/sdd-{name}"));
    }
    let mut parts = Vec::with_capacity(candidates.len());
    for rel in candidates {
        let stamp = stamp_file_with_racy_hash(&root_canon.join(&rel), 512 * 1024)?;
        parts.push(format!("{rel}={stamp}"));
    }
    let sdd_canonical = orch_core::domain::json::canonical(sdd).ok()?;
    let candidates_stamp = crate::store::sha(parts.join("\n").as_bytes());
    Some(SddArtifactsMemoKey {
        session_id: sid.to_owned(),
        project_path: root_canon.to_string_lossy().into_owned(),
        sdd_canonical,
        candidates_stamp,
    })
}

fn time_ns(t: SystemTime) -> Option<i128> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i128::try_from(d.as_nanos()).ok())
}

fn knowledge_revision_stamp(project_root: Option<&Path>) -> KnowledgeDiskStamp {
    const MAX_FILES: usize = 4096;
    let Some(root) = project_root.filter(|p| p.is_dir()) else {
        return KnowledgeDiskStamp::Absent;
    };
    let roots = [root.join("memory-bank"), root.join("project-brain")];
    if roots.iter().all(|dir| !dir.exists()) {
        return KnowledgeDiskStamp::Absent;
    }
    let mut entries_out = Vec::new();
    let mut max_mtime_ns: i128 = 0;
    for base in &roots {
        if !base.exists() {
            continue;
        }
        let Ok(base_meta) = std::fs::symlink_metadata(base) else {
            return KnowledgeDiskStamp::Racy;
        };
        if base_meta.file_type().is_symlink() || !base_meta.is_dir() {
            return KnowledgeDiskStamp::Racy;
        }
        if let Some(t) = base_meta.modified().ok().and_then(time_ns) {
            max_mtime_ns = max_mtime_ns.max(t);
        }
        let mut queue = std::collections::VecDeque::from([base.clone()]);
        while let Some(dir) = queue.pop_front() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                return KnowledgeDiskStamp::Racy;
            };
            let mut children: Vec<_> = rd.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let name = child.file_name();
                let name_s = name.to_string_lossy();
                if matches!(name_s.as_ref(), ".git" | "local")
                    || name_s.ends_with(".sqlite3-wal")
                    || name_s.ends_with(".sqlite3-shm")
                {
                    continue;
                }
                let p = child.path();
                let Ok(meta) = std::fs::symlink_metadata(&p) else {
                    return KnowledgeDiskStamp::Racy;
                };
                if meta.file_type().is_symlink() {
                    return KnowledgeDiskStamp::Racy;
                }
                if meta.is_dir() {
                    if let Some(t) = meta.modified().ok().and_then(time_ns) {
                        max_mtime_ns = max_mtime_ns.max(t);
                    }
                    queue.push_back(p);
                } else if meta.is_file() {
                    let Some(t) = meta.modified().ok().and_then(time_ns) else {
                        return KnowledgeDiskStamp::Racy;
                    };
                    max_mtime_ns = max_mtime_ns.max(t);
                    let rel = p
                        .strip_prefix(root)
                        .ok()
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|| name_s.into_owned());
                    entries_out.push(format!("{rel}:{}:{t}", meta.len()));
                    if entries_out.len() >= MAX_FILES {
                        return KnowledgeDiskStamp::Racy;
                    }
                }
            }
        }
    }
    let now_ns = time_ns(SystemTime::now()).unwrap_or(0);
    if max_mtime_ns <= 0 || now_ns <= KNOWLEDGE_RACY_NS || max_mtime_ns > now_ns - KNOWLEDGE_RACY_NS
    {
        return KnowledgeDiskStamp::Racy;
    }
    KnowledgeDiskStamp::Settled(crate::store::sha(entries_out.join("\n").as_bytes()))
}

fn resolve_knowledge_roots(
    store: &SessionStore,
    project_id: &str,
) -> (Option<PathBuf>, Option<PathBuf>, Option<String>) {
    let project_root = store
        .project(project_id)
        .ok()
        .and_then(|p| p["path"].as_str().map(PathBuf::from));
    let edition: Option<String> = None;
    let state_root = if edition.is_some() {
        Some(store.state.join("attached").join(project_id))
    } else {
        project_root.clone()
    };
    (project_root, state_root, edition)
}

fn stamp_file_with_racy_hash(path: &Path, max_hash_bytes: u64) -> Option<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Some("missing".to_owned());
        }
        Err(_) => return None,
    };
    if meta.file_type().is_symlink() {
        return None;
    }
    let mtime_ns = meta.modified().ok().and_then(time_ns)?;
    let now_ns = time_ns(SystemTime::now()).unwrap_or(0);
    let racy =
        mtime_ns <= 0 || now_ns <= KNOWLEDGE_RACY_NS || mtime_ns > now_ns - KNOWLEDGE_RACY_NS;
    if meta.is_dir() {
        return (!racy).then(|| format!("dir:{mtime_ns}"));
    }
    if !meta.is_file() {
        return None;
    }
    if racy {
        if meta.len() > max_hash_bytes {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        Some(format!(
            "file:{}:{mtime_ns}:{}",
            meta.len(),
            crate::store::sha(&bytes)
        ))
    } else {
        Some(format!("file:{}:{mtime_ns}", meta.len()))
    }
}

fn collect_preflight_request_paths(request: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for arr in [
        request["body"]["paths"].as_array(),
        request["query"]["paths"].as_array(),
    ]
    .into_iter()
    .flatten()
    {
        for raw in arr.iter().filter_map(Value::as_str) {
            if let Some(clean) = crate::services::clean_relative_path_helper(raw)
                && seen.insert(clean.clone())
            {
                paths.push(clean);
                if paths.len() >= 64 {
                    return paths;
                }
            }
        }
    }
    for text in [
        request["body"]["prompt"].as_str(),
        request["query"]["prompt"].as_str(),
        request["query"]["query"].as_str(),
    ]
    .into_iter()
    .flatten()
    {
        for token in text.split_whitespace() {
            let trimmed = token.trim_matches(|c| {
                matches!(c, '`' | '\'' | '"' | ',' | ';' | ')' | '(' | '[' | ']')
            });
            if trimmed.contains('/')
                && !trimmed.contains("://")
                && let Some(clean) = crate::services::clean_relative_path_helper(trimmed)
                && seen.insert(clean.clone())
            {
                paths.push(clean);
                if paths.len() >= 64 {
                    return paths;
                }
            }
        }
    }
    paths
}

fn workspace_paths_stamp(project_root: Option<&Path>, request: &Value) -> Option<String> {
    let requested = collect_preflight_request_paths(request);
    if requested.is_empty() {
        return Some(String::new());
    }
    let root = project_root.filter(|p| p.is_dir())?;
    let mut parts = Vec::with_capacity(requested.len());
    for rel in requested {
        let stamp = stamp_file_with_racy_hash(&root.join(&rel), 512 * 1024)?;
        parts.push(format!("{rel}={stamp}"));
    }
    Some(crate::store::sha(parts.join("\n").as_bytes()))
}

pub struct NativeServices {
    pub store: SessionStore,
    pub domains: BTreeMap<String, Arc<dyn Domain>>,
    pub bootstrap: Arc<dyn Fn() -> Result<Value> + Send + Sync>,
    pub overrides: BTreeMap<String, Arc<dyn Domain>>,
    pub closer: Arc<dyn Fn() -> CloseFuture<'static> + Send + Sync>,
    runtime_cache: Arc<Mutex<ServicesRuntimeCache>>,
}
/// The most `knowledge.sync_collect` pages one sync reads.
const SYNC_PAGES: usize = 64;
/// Every page of `knowledge.sync_collect` as one collection. `prune_layers`
/// names the layers every page reported read whole, and is empty when the
/// pages disagree on the total (knowledge changed between them) or run past
/// `SYNC_PAGES`: a sync prunes only with the whole collection in hand.
pub(crate) async fn collect_sync_items(
    knowledge: &dyn Domain,
    project_id: &str,
    query: &Value,
    body: &Value,
) -> Result<Value> {
    let mut items = Vec::new();
    let mut first = None::<Value>;
    let mut layers = None::<std::collections::BTreeSet<String>>;
    let mut offset = 0u64;
    let mut whole = false;
    for _ in 0..SYNC_PAGES {
        let mut page_body = body.clone();
        if !page_body.is_object() {
            page_body = json!({});
        }
        page_body["offset"] = json!(offset);
        let Payload::Json(page) = knowledge
            .execute(
                "knowledge.sync_collect",
                json!({"params":{"project_id":project_id},"query":query,"body":page_body}),
            )
            .await?
        else {
            return Err(HarnessError::unavailable(
                "Unexpected knowledge sync payload.",
            ));
        };
        items.extend(page["items"].as_array().cloned().unwrap_or_default());
        let reported = page["complete_layers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>();
        layers = Some(match layers {
            None => reported,
            Some(held) => held.intersection(&reported).cloned().collect(),
        });
        let consistent = first
            .as_ref()
            .is_none_or(|first| first["total_items"] == page["total_items"]);
        if first.is_none() {
            first = Some(page.clone());
        }
        if !consistent {
            break;
        }
        match page["next_offset"].as_u64() {
            Some(next) if page["has_more"] == true && next > offset => offset = next,
            _ => {
                whole = true;
                break;
            }
        }
    }
    let first = first.unwrap_or_else(|| json!({}));
    let prune_layers = if whole {
        layers.unwrap_or_default().into_iter().collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Ok(json!({
        "items": items,
        "brain_count": first["brain_count"],
        "memory_count": first["memory_count"],
        "skipped": first["skipped"],
        "total_items": first["total_items"],
        "prune_layers": prune_layers,
    }))
}
impl NativeServices {
    async fn auto_start_graph(&self, project_id: &str) {
        if !project_id.is_empty()
            && let Some(graph) = self.domains.get("graph")
        {
            let _ = graph
                .execute(
                    "graph.auto_start",
                    json!({"params":{"project_id":project_id},"body":{},"query":{}}),
                )
                .await;
        }
    }
    async fn best_effort_sync_knowledge(
        &self,
        project_id: &str,
        bank: Option<&str>,
        session_id: Option<&str>,
    ) {
        if project_id.trim().is_empty() {
            return;
        }
        let (Some(knowledge), Some(omnigraph)) =
            (self.domains.get("knowledge"), self.domains.get("omnigraph"))
        else {
            return;
        };
        if let Ok(Payload::Json(status)) = omnigraph
            .execute(
                "omnigraph.status",
                json!({"params":{},"body":{},"query":{"project_id":project_id}}),
            )
            .await
            && (status["mode"] == "off" || status["state"] == "unconfigured")
        {
            return;
        }
        let (project_root, state_root, _) = resolve_knowledge_roots(&self.store, project_id);
        let stamp_root = state_root.as_deref().or(project_root.as_deref());
        let disk_stamp = knowledge_revision_stamp(stamp_root);
        let bank_norm = bank.map(str::trim).unwrap_or("").to_owned();
        let session_norm = session_id.map(str::trim).unwrap_or("").to_owned();
        let sync_key = KnowledgeSyncStampKey {
            project_id: project_id.to_owned(),
            bank: bank_norm,
            session_id: session_norm,
        };
        let settled_token = match &disk_stamp {
            KnowledgeDiskStamp::Settled(stamp) => self.runtime_cache.lock().ok().map(|guard| {
                let (_, knowledge_epoch, _, _) = guard.epochs_for(project_id);
                (stamp.clone(), knowledge_epoch)
            }),
            KnowledgeDiskStamp::Absent | KnowledgeDiskStamp::Racy => None,
        };
        if let Some(ref token) = settled_token
            && let Ok(mut guard) = self.runtime_cache.lock()
            && guard
                .knowledge_sync_stamps
                .get(&sync_key)
                .is_some_and(|(prev, _)| prev == token)
        {
            guard.seq = guard.seq.saturating_add(1);
            let s = guard.seq;
            if let Some(entry) = guard.knowledge_sync_stamps.get_mut(&sync_key) {
                entry.1 = s;
            }
            return;
        }
        let mut collect_body = json!({"project_id": project_id});
        if let Some(b) = bank.map(str::trim).filter(|s| !s.is_empty()) {
            collect_body["bank"] = json!(b);
        }
        if let Some(s) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
            collect_body["session_id"] = json!(s);
        }
        let Ok(collected) =
            collect_sync_items(knowledge.as_ref(), project_id, &json!({}), &collect_body).await
        else {
            return;
        };
        let sync_res = omnigraph
            .execute(
                "omnigraph.sync_items",
                json!({"params":{},"query":{},"body":{"project_id":project_id,"items":collected["items"],"prune_layers":collected["prune_layers"]}}),
            )
            .await;
        if sync_res.is_ok()
            && let Ok(mut guard) = self.runtime_cache.lock()
        {
            guard.bump_omnigraph(project_id);
            if let Some(token) = settled_token {
                guard.seq = guard.seq.saturating_add(1);
                let s = guard.seq;
                if guard.knowledge_sync_stamps.len() >= MAX_KNOWLEDGE_SYNC_STAMPS
                    && !guard.knowledge_sync_stamps.contains_key(&sync_key)
                    && let Some(evict) = guard
                        .knowledge_sync_stamps
                        .iter()
                        .min_by_key(|(_, (_, last))| *last)
                        .map(|(k, _)| k.clone())
                {
                    guard.knowledge_sync_stamps.remove(&evict);
                }
                guard.knowledge_sync_stamps.insert(sync_key, (token, s));
            }
        }
    }
    async fn attach_waiting_permission_counts(&self, sessions: &mut [Value]) {
        let counts = self.waiting_permission_counts().await;
        for summary in sessions {
            let count = summary["id"]
                .as_str()
                .and_then(|id| counts.get(id))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            summary["waiting_permissions"] = json!(count);
        }
    }
    async fn handle_bootstrap(&self) -> Result<Payload> {
        let mut data = (self.bootstrap)()?;
        if let Some(graph) = self.domains.get("graph") {
            let _ = graph
                .execute(
                    "graph.auto_start_all",
                    json!({"params":{},"body":{},"query":{}}),
                )
                .await;
        }
        if let Some(omnigraph) = self.domains.get("omnigraph")
            && let Ok(Payload::Json(status)) = omnigraph
                .execute(
                    "omnigraph.status",
                    json!({"params":{},"body":{},"query":{}}),
                )
                .await
            && status.get("state").is_some()
        {
            data["omnigraph"] = status;
        }
        if let Some(sessions) = data["sessions"].as_array_mut() {
            self.attach_waiting_permission_counts(sessions).await;
        }
        Ok(Payload::Json(data))
    }
    async fn handle_sessions_list(&self, params: &Value, query: &Value) -> Result<Payload> {
        if let Some(project_id) = query["project_id"]
            .as_str()
            .or_else(|| params["project_id"].as_str())
            .filter(|id| !id.is_empty())
        {
            self.auto_start_graph(project_id).await;
        }
        let mut summaries = self.store.summaries()?;
        self.attach_waiting_permission_counts(&mut summaries).await;
        Ok(Payload::Json(json!({"sessions": summaries})))
    }
    async fn handle_sessions_get(&self, sid: &str, query: &Value) -> Result<Payload> {
        let parse_opt_i64 = |key: &str| -> Result<Option<i64>> {
            match query[key].as_str() {
                None => Ok(None),
                Some(raw) => raw
                    .parse::<i64>()
                    .map(Some)
                    .map_err(|_| HarnessError::invalid("Invalid event cursor.")),
            }
        };
        let after = parse_opt_i64("after")?;
        let before = parse_opt_i64("before")?;
        let limit = match query["limit"].as_str() {
            None => crate::store::DEFAULT_EVENT_PAGE_LIMIT,
            Some(raw) => {
                let parsed = raw
                    .parse::<usize>()
                    .map_err(|_| HarnessError::invalid("Invalid event page limit."))?;
                if !(1..=crate::store::MAX_EVENT_PAGE_LIMIT).contains(&parsed) {
                    return Err(HarnessError::invalid(format!(
                        "Event page limit must be between 1 and {}.",
                        crate::store::MAX_EVENT_PAGE_LIMIT
                    )));
                }
                parsed
            }
        };
        let tail = match query["tail"].as_str() {
            None | Some("0" | "false" | "no" | "") => false,
            Some("1" | "true" | "yes") => true,
            Some(_) => return Err(HarnessError::invalid("Invalid tail pagination flag.")),
        };
        let page = self
            .store
            .session_events_page(sid, after, before, limit, tail)?;
        let mut session = page.session;
        session["graph_focus"] = crate::graph_live::session_focus_value(&self.store, sid)?;
        if after.is_none_or(|v| v == 0)
            && before.is_none()
            && let Some(project_id) = session["project_id"].as_str()
        {
            self.auto_start_graph(project_id).await;
        }
        let waiting = self.waiting_permissions_for(sid).await;
        Ok(Payload::Json(json!({
            "session": session,
            "events": page.events,
            "waiting_permissions": waiting,
            "has_more": page.has_more,
            "has_older": page.has_older,
            "total_events": page.total_events,
            "oldest_event_id": page.oldest_event_id,
            "latest_event_id": page.latest_event_id,
        })))
    }
    #[cfg_attr(not(test), allow(dead_code))]
    fn emit_domain_mutation_sse(
        &self,
        operation: &str,
        params: &Value,
        body: &Value,
        payload: &Payload,
    ) {
        self.emit_domain_mutation_sse_with_query(operation, params, &Value::Null, body, payload);
    }

    fn resolve_project_id(&self, candidates: &[Option<&Value>]) -> Option<String> {
        candidates
            .iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .map(str::to_owned)
            .next()
    }

    fn resolve_project_id_for_config_path(&self, config_path: &str) -> Option<String> {
        let trimmed = config_path.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let projects = self.store.projects().ok()?;
        projects
            .into_iter()
            .filter_map(|project| {
                let id = project["id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())?
                    .to_owned();
                let root = project["path"]
                    .as_str()
                    .or_else(|| project["root"].as_str())
                    .map(|s| s.trim().trim_end_matches('/'))
                    .filter(|s| !s.is_empty())?;
                let matches = trimmed == root
                    || (trimmed.starts_with(root)
                        && trimmed.as_bytes().get(root.len()).copied() == Some(b'/'));
                matches.then_some((root.len(), id))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, id)| id)
    }

    fn emit_domain_mutation_sse_with_query(
        &self,
        operation: &str,
        params: &Value,
        query: &Value,
        body: &Value,
        payload: &Payload,
    ) {
        let Payload::Json(val) = payload else {
            return;
        };
        match operation {
            "creator.start" | "creator.act" => {
                let run = val.get("run").filter(|v| v.is_object()).unwrap_or(val);
                let Some(run_id) = run["id"].as_str().filter(|s| !s.is_empty()) else {
                    return;
                };
                let sid = run["session_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let project_id = run["project_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let status = run["status"].as_str().unwrap_or("new");
                let active = run["active"].as_bool().unwrap_or(matches!(
                    status,
                    "scanning" | "generating" | "applying" | "rolling_back" | "queued" | "running"
                ));
                let waiting = run["waiting_permissions"]
                    .as_u64()
                    .map(|n| n as usize)
                    .unwrap_or_else(|| sid.as_deref().map_or(0, |id| self.store.waiting_count(id)));
                let mut ev = json!({
                    "run_id": run_id,
                    "id": run_id,
                    "project_id": project_id,
                    "status": status,
                    "active": active,
                    "waiting_permissions": waiting,
                    "session_id": sid,
                    "run": run,
                });
                if let Some(rev) = run["revision"].as_u64() {
                    ev["revision"] = json!(rev);
                }
                self.store.publish_event("creator_run", sid, project_id, ev);
            }
            "systems.prepare" | "systems.act" | "systems.apply" | "discovery.start" => {
                let run = val.get("run").filter(|v| v.is_object()).unwrap_or(val);
                let run_id = run["id"]
                    .as_str()
                    .or_else(|| run["run_id"].as_str())
                    .or_else(|| params["id"].as_str())
                    .filter(|s| !s.is_empty());
                let Some(run_id) = run_id else {
                    return;
                };
                let sid = run["session_id"]
                    .as_str()
                    .or_else(|| val["session"]["id"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let project_id = run["project_id"]
                    .as_str()
                    .or_else(|| body["project_id"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let status = run["status"]
                    .as_str()
                    .or_else(|| val["status"].as_str())
                    .unwrap_or("queued");
                let active = run["active"]
                    .as_bool()
                    .unwrap_or(matches!(status, "queued" | "running"));
                let waiting = run["waiting_permissions"]
                    .as_u64()
                    .map(|n| n as usize)
                    .unwrap_or_else(|| sid.as_deref().map_or(0, |id| self.store.waiting_count(id)));
                let ev = json!({
                    "run_id": run_id,
                    "id": run_id,
                    "project_id": project_id,
                    "status": status,
                    "active": active,
                    "waiting_permissions": waiting,
                    "session_id": sid,
                    "run": run,
                });
                self.store.publish_event("system_run", sid, project_id, ev);
            }
            "results.start_check" => {
                let explicit_check = val.get("check").filter(|v| v.is_object());
                let session = val.get("session").filter(|v| v.is_object());
                let explicit_id = explicit_check
                    .and_then(|c| c["id"].as_str())
                    .or_else(|| val["id"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let sid = explicit_check
                    .and_then(|c| c["session_id"].as_str())
                    .or_else(|| val["session_id"].as_str())
                    .or_else(|| {
                        session.and_then(|s| s["id"].as_str().or_else(|| s["session_id"].as_str()))
                    })
                    .or_else(|| params["session_id"].as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("")
                    .to_owned();
                let mut db_check_val: Option<Value> = None;
                if explicit_id.is_none()
                    && !sid.is_empty()
                    && let Ok(db) = self.store.connection()
                {
                    let has_checks = db
                        .query_row(
                            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='checks'",
                            [],
                            |r| r.get::<_, i64>(0),
                        )
                        .is_ok();
                    if has_checks
                        && let Ok((row_id, raw_data)) = db.query_row(
                            "SELECT id, data FROM checks WHERE session_id = ?1 ORDER BY rowid DESC LIMIT 1",
                            [&sid],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                        )
                    {
                        let mut parsed = serde_json::from_str::<Value>(&raw_data)
                            .unwrap_or_else(|_| json!({}));
                        if parsed["id"].as_str().is_none_or(str::is_empty) {
                            parsed["id"] = json!(row_id);
                        }
                        db_check_val = Some(parsed);
                    }
                }
                let latest_session_check =
                    session
                        .and_then(|s| s["checks"].as_array())
                        .and_then(|checks| {
                            checks.iter().rev().find(|c| {
                                c.is_object() && c["id"].as_str().is_some_and(|s| !s.is_empty())
                            })
                        });
                let check_id = explicit_id
                    .or_else(|| {
                        db_check_val
                            .as_ref()
                            .and_then(|c| c["id"].as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                    .or_else(|| {
                        latest_session_check
                            .and_then(|c| c["id"].as_str())
                            .map(str::to_owned)
                    });
                if check_id.is_none() && session.is_none() && sid.is_empty() {
                    return;
                }
                let check_obj = explicit_check
                    .or(db_check_val.as_ref())
                    .or(latest_session_check)
                    .unwrap_or(val);
                let project_id = (!sid.is_empty())
                    .then(|| self.store.session_project_id(&sid))
                    .flatten()
                    .or_else(|| {
                        session
                            .and_then(|s| s["project_id"].as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                    .or_else(|| {
                        val["project_id"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    });
                let status = explicit_check
                    .and_then(|c| c["status"].as_str())
                    .or_else(|| db_check_val.as_ref().and_then(|c| c["status"].as_str()))
                    .or_else(|| latest_session_check.and_then(|c| c["status"].as_str()))
                    .or_else(|| val["status"].as_str())
                    .or_else(|| session.and_then(|s| s["status"].as_str()))
                    .unwrap_or("queued");
                self.store.publish_event(
                    "results_check",
                    (!sid.is_empty()).then_some(sid.clone()),
                    project_id.clone(),
                    json!({
                        "id": check_id,
                        "check_id": check_id,
                        "session_id": sid,
                        "project_id": project_id,
                        "status": status,
                        "check": check_obj,
                        "session": session,
                    }),
                );
            }
            "graph.act" => {
                if body["action"].as_str() == Some("changes") {
                    return;
                }
                let Some(project_id) = self.resolve_project_id(&[
                    val.get("project_id"),
                    params.get("project_id"),
                    query.get("project_id"),
                    body.get("project_id"),
                ]) else {
                    return;
                };
                let state = val["state"]
                    .as_str()
                    .or_else(|| body["action"].as_str())
                    .unwrap_or("updated");
                let mut ev = val.clone();
                if ev.get("project_id").is_none() {
                    ev["project_id"] = json!(project_id);
                }
                if ev.get("state").is_none() {
                    ev["state"] = json!(state);
                }
                if ev.get("reason").is_none() {
                    ev["reason"] = json!(body["action"].as_str().unwrap_or("graph_act"));
                }
                if ev.get("revision").is_none()
                    && let Some(ir_rev) = val.get("ir_revision").and_then(Value::as_u64)
                {
                    ev["revision"] = json!(ir_rev);
                }
                if ev.get("ir_revision").is_none()
                    && let Some(rev) = ev.get("revision").and_then(Value::as_u64)
                {
                    ev["ir_revision"] = json!(rev);
                }
                self.store
                    .publish_event("graph_status", None, Some(project_id), ev);
            }
            "system_graph.act" | "system_graph.service_act" => {
                let config_val = val
                    .get("config_path")
                    .or_else(|| body.get("config_path"))
                    .or_else(|| query.get("config_path"))
                    .cloned()
                    .unwrap_or_else(|| body["config_path"].clone());
                let project_id = self
                    .resolve_project_id(&[
                        val.get("project_id"),
                        val.get("system").and_then(|sys| sys.get("project_id")),
                        params.get("project_id"),
                        query.get("project_id"),
                        body.get("project_id"),
                    ])
                    .or_else(|| {
                        config_val
                            .as_str()
                            .and_then(|cp| self.resolve_project_id_for_config_path(cp))
                    });
                let Some(project_id) = project_id else {
                    return;
                };
                let action = body["action"].as_str().unwrap_or("updated");
                let service = val["service"]
                    .as_str()
                    .or_else(|| body["service"].as_str())
                    .filter(|s| !s.is_empty());
                let state = val["state"].as_str().unwrap_or(action);
                let mut ev = json!({
                    "project_id": project_id,
                    "service": service,
                    "config_path": config_val,
                    "state": state,
                    "action": action,
                    "reason": action,
                });
                if let Some(rev) = val.get("ir_revision").and_then(Value::as_u64) {
                    ev["ir_revision"] = json!(rev);
                }
                self.store
                    .publish_event("system_graph_status", None, Some(project_id), ev);
            }
            "omnigraph.setup"
            | "omnigraph.branch_act"
            | "omnigraph.publish"
            | "omnigraph.import"
            | "omnigraph.sync_knowledge"
            | "omnigraph.promote" => {
                let project_id = self.resolve_project_id(&[
                    val.get("project_id"),
                    params.get("project_id"),
                    query.get("project_id"),
                    body.get("project_id"),
                ]);
                let branch = val["branch"]
                    .as_str()
                    .or_else(|| body["branch"].as_str())
                    .or_else(|| body["target_branch"].as_str())
                    .filter(|s| !s.is_empty());
                let state = val["state"].as_str().unwrap_or("updated");
                let sid = body["session_id"]
                    .as_str()
                    .or_else(|| val["session"]["id"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                self.store.publish_event(
                    "omnigraph_status",
                    sid,
                    project_id.clone(),
                    json!({
                        "project_id": project_id,
                        "branch": branch,
                        "operation": operation,
                        "state": state,
                    }),
                );
            }
            _ => {}
        }
    }
    async fn handle_memory_links(&self, operation: &str, request: &Value) -> Result<Payload> {
        let knowledge = self
            .domains
            .get("knowledge")
            .ok_or_else(|| HarnessError::unavailable("Required native service is unavailable."))?;
        let Payload::Json(mut links) = knowledge.execute(operation, request.clone()).await? else {
            return Err(HarnessError::unavailable(
                "Unexpected memory links payload.",
            ));
        };
        let project_id = links
            .get("project_id")
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            .cloned()
            .unwrap_or_else(|| request["params"]["project_id"].clone());
        if operation == "knowledge.memory_links_batch" {
            if let Some(items) = links["items"].as_array_mut() {
                self.enrich_items_concurrently(&project_id, items).await;
            }
        } else {
            self.enrich_items_concurrently(&project_id, std::slice::from_mut(&mut links))
                .await;
        }
        Ok(Payload::Json(links))
    }
    async fn handle_preflight(&self, request: &Value) -> Result<Payload> {
        Box::pin(async move {
            let pid_str = request["params"]["project_id"]
                .as_str()
                .or_else(|| request["query"]["project_id"].as_str())
                .or_else(|| request["body"]["project_id"].as_str())
                .unwrap_or("")
                .trim()
                .to_owned();
            let memo_key = self.build_preflight_memo_key(&pid_str, request);
            let mut preflight = if let Some(ref key) = memo_key
                && let Ok(mut guard) = self.runtime_cache.lock()
                && let Some(cached) = guard.get_preflight(key)
            {
                cached
            } else {
                let knowledge = self.domains.get("knowledge").ok_or_else(|| {
                    HarnessError::unavailable("Required native service is unavailable.")
                })?;
                let Payload::Json(mut computed) =
                    Box::pin(knowledge.execute("knowledge.preflight", request.clone())).await?
                else {
                    return Err(HarnessError::unavailable("Unexpected preflight payload."));
                };
                let project_id = computed
                    .get("project_id")
                    .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                    .cloned()
                    .unwrap_or_else(|| request["params"]["project_id"].clone());
                let mut contradictions = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                if let Some(files) = computed["files"].as_array_mut() {
                    Box::pin(self.enrich_items_concurrently(&project_id, files)).await;
                    for item in files {
                        collect_unique_contradictions(item, &mut contradictions, &mut seen);
                    }
                }
                if computed.is_object() {
                    computed["contradiction_warnings"] = Value::Array(contradictions);
                }
                let has_omnigraph_error = computed["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|item| item.get("omnigraph_error").is_some_and(|v| !v.is_null()));
                if let Some(key) = memo_key
                    && computed.is_object()
                    && computed["ok"].as_bool().unwrap_or(true)
                    && !has_omnigraph_error
                    && let Ok(mut guard) = self.runtime_cache.lock()
                {
                    guard.insert_preflight(key, computed.clone());
                }
                computed
            };
            if preflight.is_object()
                && !request["body"]["skillcache"].is_null()
                && let Some(sessions) = self.domains.get("sessions")
                && let Ok(Payload::Json(preview)) =
                    Box::pin(sessions.execute("sessions.skillcache_preview", request.clone())).await
            {
                // What SkillCache would decide for this message, evaluated dynamically outside base memo.
                preflight["skillcache"] = preview;
            }
            Ok(Payload::Json(preflight))
        })
        .await
    }
    async fn annotate_delivery_preview(&self, sid: &str, preview: &mut Value) {
        let mut contradiction_warnings = Vec::new();
        let mut hygiene_warnings = Vec::new();
        let mut risk_callers = Vec::new();

        let paths = extract_delivery_preview_paths(preview);
        let project_id = (!sid.is_empty())
            .then(|| self.store.get(sid).ok())
            .flatten()
            .and_then(|s| s["project_id"].as_str().map(str::to_owned));

        if let Some(project_id) = project_id.filter(|id| !id.is_empty())
            && !paths.is_empty()
        {
            let preflight_req = json!({
                "params": { "project_id": project_id },
                "query": {},
                "body": {
                    "paths": paths,
                    "risk_caller_hints": false,
                },
            });
            let mut enriched_via_preflight = false;
            if let Ok(Payload::Json(pf)) = Box::pin(self.handle_preflight(&preflight_req)).await
                && pf["files"].is_array()
            {
                enriched_via_preflight = true;
                if let Some(items) = pf["contradiction_warnings"].as_array() {
                    contradiction_warnings = items.clone();
                }
                if let Some(items) = pf["hygiene_warnings"].as_array() {
                    hygiene_warnings = items.clone();
                }
                if let Some(items) = pf["risk_callers"].as_array() {
                    risk_callers = items.clone();
                }
            }
            if !enriched_via_preflight {
                let mut seen = std::collections::BTreeSet::new();
                let mut items: Vec<Value> = paths
                    .iter()
                    .map(|path| json!({ "path": path, "contradiction_warnings": [] }))
                    .collect();
                self.enrich_items_concurrently(&json!(project_id), &mut items)
                    .await;
                for item in &items {
                    collect_unique_contradictions(item, &mut contradiction_warnings, &mut seen);
                }
            }
        }

        preview["memory_warnings"] = json!({
            "contradiction_warnings": contradiction_warnings,
            "hygiene_warnings": hygiene_warnings,
        });
        preview["risk_callers"] = Value::Array(risk_callers);
    }
    async fn handle_delivery(&self, operation: &str, request: Value) -> Result<Payload> {
        let domain = self
            .domains
            .get("delivery")
            .ok_or_else(|| HarnessError::unavailable("Required native service is unavailable."))?;
        let sid = request["params"]["session_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let action = request["body"]["action"].as_str().unwrap_or("").to_owned();
        let mut payload = domain.execute(operation, request).await?;
        if let Payload::Json(ref mut val) = payload {
            if operation == "delivery.preview_commit"
                || operation == "delivery.preview_transfer"
                || (operation == "delivery.act"
                    && matches!(action.as_str(), "preview_commit" | "preview_transfer"))
            {
                Box::pin(self.annotate_delivery_preview(&sid, val)).await;
            } else if operation == "delivery.get"
                && let Some(preview) = val.get_mut("preview").filter(|v| v.is_object())
            {
                Box::pin(self.annotate_delivery_preview(&sid, preview)).await;
            }
        }
        Ok(payload)
    }
    async fn waiting_permission_counts(&self) -> serde_json::Map<String, Value> {
        let Some(sessions) = self.domains.get("sessions") else {
            return serde_json::Map::new();
        };
        match sessions
            .execute(
                "sessions.waiting_counts",
                json!({"params":{},"query":{},"body":{}}),
            )
            .await
        {
            Ok(Payload::Json(waiting)) => {
                waiting["counts"].as_object().cloned().unwrap_or_default()
            }
            _ => serde_json::Map::new(),
        }
    }
    async fn handle_permissions_waiting(&self, query: &Value) -> Result<Payload> {
        let limit = match query["limit"].as_str() {
            None | Some("") => 50usize,
            Some(raw) => {
                let parsed = raw
                    .parse::<usize>()
                    .map_err(|_| HarnessError::invalid("Invalid waiting permissions limit."))?;
                if !(1..=200).contains(&parsed) {
                    return Err(HarnessError::invalid(
                        "Waiting permissions limit must be between 1 and 200.",
                    ));
                }
                parsed
            }
        };
        let filter_project = query["project_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let filter_sids = match query["session_id"].as_str().map(str::trim) {
            None | Some("") => None,
            Some(raw) => {
                let mut seen = std::collections::BTreeSet::new();
                let mut ids = Vec::new();
                for part in raw.split(',') {
                    let sid = part.trim();
                    if sid.is_empty() || sid.len() > 200 || sid.chars().any(char::is_control) {
                        return Err(HarnessError::invalid("Invalid session_id filter."));
                    }
                    if seen.insert(sid.to_owned()) {
                        ids.push(sid.to_owned());
                    }
                    if ids.len() > 200 {
                        return Err(HarnessError::invalid(
                            "Too many session_id values requested.",
                        ));
                    }
                }
                Some(ids)
            }
        };

        let counts = self.waiting_permission_counts().await;
        let total: usize = counts.values().filter_map(Value::as_u64).sum::<u64>() as usize;
        let candidate_sids: Vec<String> = if let Some(explicit) = filter_sids {
            explicit.into_iter().take(limit).collect()
        } else {
            counts
                .iter()
                .filter_map(|(sid, count)| count.as_u64().filter(|&n| n > 0).map(|_| sid.clone()))
                .collect()
        };

        let mut meta_by_sid: HashMap<String, (String, String)> = HashMap::new();
        if !candidate_sids.is_empty()
            && let Ok(db) = self.store.connection()
            && let Ok(mut stmt) = db.prepare("SELECT project_id, title FROM sessions WHERE id = ?1")
        {
            for sid in &candidate_sids {
                if let Ok((proj_id, title)) = stmt.query_row([sid], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                }) {
                    meta_by_sid.insert(sid.clone(), (proj_id, title));
                }
            }
        }

        let mut sessions_out = Vec::new();
        for sid in candidate_sids {
            let (proj_id, title) = meta_by_sid.get(&sid).cloned().unwrap_or_default();
            if let Some(ref want_project) = filter_project
                && !proj_id.is_empty()
                && &proj_id != want_project
            {
                continue;
            }
            if filter_project.is_some() && proj_id.is_empty() {
                continue;
            }
            let items = self.waiting_permissions_for(&sid).await;
            let count = items.len();
            sessions_out.push(json!({
                "session_id": sid,
                "project_id": proj_id,
                "title": title,
                "count": count,
                "waiting_permissions": items,
            }));
            if sessions_out.len() >= limit {
                break;
            }
        }
        Ok(Payload::Json(json!({
            "counts": counts,
            "total": total,
            "sessions": sessions_out,
        })))
    }
    async fn waiting_permissions_for(&self, sid: &str) -> Vec<Value> {
        let Some(sessions) = self.domains.get("sessions") else {
            return Vec::new();
        };
        match sessions
            .execute(
                "sessions.waiting_permissions",
                json!({"params":{"session_id":sid},"query":{},"body":{}}),
            )
            .await
        {
            Ok(Payload::Json(waiting)) => waiting["items"].as_array().cloned().unwrap_or_default(),
            _ => Vec::new(),
        }
    }
    async fn enrich_items_concurrently(&self, project_id: &Value, items: &mut [Value]) {
        let omnigraph = self.domains.get("omnigraph").cloned();
        if items.len() <= 1 || tokio::runtime::Handle::try_current().is_err() {
            for item in items {
                let path = item["path"].clone();
                let res =
                    fetch_omnigraph_enrichment(omnigraph.clone(), project_id.clone(), path).await;
                apply_omnigraph_enrichment(item, res);
            }
            return;
        }
        let mut set: tokio::task::JoinSet<(usize, OmnigraphEnrichment)> =
            tokio::task::JoinSet::new();
        let mut next_idx = 0usize;
        while next_idx < items.len() && set.len() < MAX_CONCURRENT_FILE_CONTEXT {
            let idx = next_idx;
            next_idx += 1;
            let og = omnigraph.clone();
            let pid = project_id.clone();
            let path = items[idx]["path"].clone();
            set.spawn(async move { (idx, fetch_omnigraph_enrichment(og, pid, path).await) });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((idx, enrichment)) = joined {
                apply_omnigraph_enrichment(&mut items[idx], enrichment);
            }
            if next_idx < items.len() {
                let idx = next_idx;
                next_idx += 1;
                let og = omnigraph.clone();
                let pid = project_id.clone();
                let path = items[idx]["path"].clone();
                set.spawn(async move { (idx, fetch_omnigraph_enrichment(og, pid, path).await) });
            }
        }
    }
    fn build_preflight_memo_key(
        &self,
        project_id: &str,
        request: &Value,
    ) -> Option<PreflightMemoKey> {
        if project_id.is_empty() {
            return None;
        }
        let requested_sid = [
            request["params"]["session_id"].as_str(),
            request["query"]["session_id"].as_str(),
            request["body"]["session_id"].as_str(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty());

        let (project_root, state_root, edition) = resolve_knowledge_roots(&self.store, project_id);
        let (omnigraph_epoch, knowledge_epoch, graph_epoch, session_epoch) = self
            .runtime_cache
            .lock()
            .ok()
            .map(|guard| guard.epochs_for(project_id))
            .unwrap_or((0, 0, 0, 0));

        let (effective_project_root, effective_state_root, session_key) =
            if let Some(sid) = requested_sid {
                let session = self.store.get(sid).ok()?;
                if session["project_id"].as_str() != Some(project_id) {
                    return None;
                }
                let session_workspace = session["project_path"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir());
                let eff_proj = session_workspace.or(project_root);
                let eff_state = if edition.is_some() {
                    state_root
                } else {
                    eff_proj.clone()
                };
                let latest_event_id = self
                    .store
                    .connection()
                    .ok()
                    .and_then(|db| {
                        db.query_row(
                            "SELECT MAX(id) FROM events WHERE session_id=?1",
                            [sid],
                            |r| r.get::<_, Option<i64>>(0),
                        )
                        .ok()
                        .flatten()
                    })
                    .unwrap_or(0);
                let stamp_doc = json!({
                    "project_path": session["project_path"],
                    "status": session["status"],
                    "updated_at": session["updated_at"],
                    "brain": session["brain"],
                    "latest_event_id": latest_event_id,
                });
                let session_stamp = orch_core::domain::json::canonical(&stamp_doc)
                    .ok()
                    .map(|c| crate::store::sha(c.as_bytes()))?;
                (
                    eff_proj,
                    eff_state,
                    Some(PreflightSessionKey {
                        session_id: sid.to_owned(),
                        session_epoch,
                        session_stamp,
                    }),
                )
            } else {
                (project_root, state_root, None)
            };

        let stamp_root = effective_state_root.as_deref();
        let knowledge_stamp = match knowledge_revision_stamp(stamp_root) {
            KnowledgeDiskStamp::Racy => return None,
            KnowledgeDiskStamp::Absent => {
                if edition.is_some() {
                    // Attached project without materialized bank/brain yet -> bypass cleanly.
                    return None;
                }
                "absent".to_owned()
            }
            KnowledgeDiskStamp::Settled(digest) => digest,
        };
        let live_graph_path = self
            .store
            .state
            .join("graphs")
            .join(project_id)
            .join("live")
            .join("graph.json");
        let live_graph_stamp = stamp_file_with_racy_hash(&live_graph_path, 2 * 1024 * 1024)?;
        let workspace_paths_stamp =
            workspace_paths_stamp(effective_project_root.as_deref(), request)?;
        let mut req_norm = request.clone();
        if let Some(body) = req_norm.get_mut("body").and_then(Value::as_object_mut) {
            body.remove("skillcache");
            if session_key.is_some() {
                body.remove("session_id");
            }
        }
        if session_key.is_some() {
            for container in ["params", "query"] {
                if let Some(obj) = req_norm.get_mut(container).and_then(Value::as_object_mut) {
                    obj.remove("session_id");
                }
            }
        }
        let request_sha = orch_core::domain::json::canonical(&req_norm)
            .ok()
            .map(|c| crate::store::sha(c.as_bytes()))?;
        let ir_revision =
            crate::graph_ir::GraphIrStore::open_readonly_cached(&self.store.state, project_id)
                .ok()
                .flatten()
                .map(|ir| ir.current_write_epoch())
                .unwrap_or(0);
        let ir_bus_epoch = ir_bus_epochs()
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .get(&(self.store.state.clone(), project_id.to_owned()))
                    .copied()
            })
            .unwrap_or(0);
        Some(PreflightMemoKey {
            base: PreflightBaseKey {
                project_id: project_id.to_owned(),
                request_sha,
                ir_revision,
                omnigraph_epoch,
                knowledge_epoch,
                graph_epoch,
                ir_bus_epoch,
                edition: edition.unwrap_or_default(),
                knowledge_stamp,
                live_graph_stamp,
                workspace_paths_stamp,
            },
            session: session_key,
        })
    }
    pub fn new(
        store: SessionStore,
        domains: BTreeMap<String, Arc<dyn Domain>>,
        bootstrap: Arc<dyn Fn() -> Result<Value> + Send + Sync>,
        overrides: BTreeMap<String, Arc<dyn Domain>>,
        closer: Arc<dyn Fn() -> CloseFuture<'static> + Send + Sync>,
    ) -> Result<Self> {
        let required = [
            "sessions",
            "systems",
            "discovery",
            "results",
            "delivery",
            "projects",
            "commands",
            "files",
            "providers",
            "omnigraph",
            "working_memory",
        ];
        if required.iter().any(|name| !domains.contains_key(*name)) {
            return Err(HarnessError::invalid(
                "Every native Harness domain must be supplied before serving HTTP.",
            ));
        }
        Ok(Self {
            store,
            domains,
            bootstrap,
            overrides,
            closer,
            runtime_cache: Arc::new(Mutex::new(ServicesRuntimeCache::default())),
        })
    }
}
fn extract_delivery_preview_paths(preview: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(files) = preview["files"].as_array() {
        for entry in files {
            let Some(clean) = entry
                .as_str()
                .or_else(|| entry["path"].as_str())
                .and_then(crate::services::clean_relative_path_helper)
            else {
                continue;
            };
            if seen.insert(clean.clone()) {
                paths.push(clean);
                if paths.len() >= 24 {
                    break;
                }
            }
        }
    }
    paths
}
fn collect_unique_contradictions(
    item: &Value,
    out: &mut Vec<Value>,
    seen: &mut std::collections::BTreeSet<(String, String)>,
) {
    for warning in item["contradiction_warnings"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let key = (
            warning["source_id"].as_str().unwrap_or("").to_owned(),
            warning["target_id"].as_str().unwrap_or("").to_owned(),
        );
        if seen.insert(key) {
            let mut annotated = warning.clone();
            if annotated.get("path").is_none() && item["path"].is_string() {
                annotated["path"] = item["path"].clone();
            }
            out.push(annotated);
        }
    }
}
impl Services for NativeServices {
    fn close<'a>(&'a self) -> CloseFuture<'a> {
        (self.closer)()
    }
    fn subscribe_stream(&self, last_event_id: Option<u64>) -> Option<crate::store::ReplayStart> {
        ensure_graph_event_bridge(&self.store);
        Some(self.store.stream().subscribe(last_event_id))
    }
    fn allows_sse_graph_project(&self, subscribed_pid: &str, event_pid: &str) -> bool {
        let sub = subscribed_pid.trim();
        let ev = event_pid.trim();
        if sub.is_empty() || ev.is_empty() {
            return false;
        }
        if sub == ev {
            return true;
        }
        related_system_projects_for(&self.store, sub).contains(ev)
    }
    fn request<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a> {
        ensure_graph_event_bridge(&self.store);
        Box::pin(async move {
            let params = &request["params"];
            let query = &request["query"];
            let body = &request["body"];
            let sid = params["session_id"].as_str().unwrap_or("");
            let payload = if let Some(domain) = self.overrides.get(operation) {
                domain.execute(operation, request.clone()).await?
            } else {
                match operation {
                    "bootstrap" => Box::pin(self.handle_bootstrap()).await?,
                    "sessions.list" => Box::pin(self.handle_sessions_list(params, query)).await?,
                    "sessions.get" => Box::pin(self.handle_sessions_get(sid, query)).await?,
                    "sessions.waiting_permissions" => {
                        if self.store.session_project_id(sid).is_none() {
                            return Err(HarnessError::missing("Session not found."));
                        }
                        let items = self.waiting_permissions_for(sid).await;
                        Payload::Json(json!({
                            "session_id": sid,
                            "items": items.clone(),
                            "waiting_permissions": items,
                        }))
                    }
                    "permissions.waiting" => {
                        Box::pin(self.handle_permissions_waiting(query)).await?
                    }
                    "sessions.merge_archive" => Payload::Json(self.store.merge_archive(sid)?),
                    "sessions.delete_merge" => {
                        if !crate::store::exact(body, &[], &[]) {
                            return Err(HarnessError::invalid(
                                "Invalid saved context deletion request.",
                            ));
                        }
                        Payload::Json(json!({"session":self.store.delete_merge(sid)?}))
                    }
                    "sessions.report" => {
                        let session = self.store.get(sid)?;
                        if session["status"] != "completed" {
                            return Err(HarnessError::conflict(
                                "An approved Fleet report is not available.",
                            ));
                        }
                        let text = session["fleet_result"]["report"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| {
                                HarnessError::missing("An approved Fleet report is not available.")
                            })?;
                        Payload::Bytes {
                            bytes: text.as_bytes().to_vec(),
                            content_type: "text/markdown; charset=utf-8".into(),
                            filename: None,
                            etag: None,
                            cache: None,
                        }
                    }
                    "attachments.read" => {
                        let (item, bytes, _) = crate::attachments::read(
                            &self.store,
                            sid,
                            params["id"].as_str().unwrap_or(""),
                        )?;
                        Payload::Bytes {
                            bytes,
                            content_type: "application/octet-stream".into(),
                            filename: item["name"].as_str().map(String::from),
                            etag: None,
                            cache: None,
                        }
                    }
                    "projects.list" => {
                        Payload::Json(json!({"projects":(self.bootstrap)()?["projects"]}))
                    }
                    "projects.add" => {
                        if !crate::store::exact(body, &["path"], &[]) {
                            return Err(HarnessError::invalid(
                                "Invalid project registration request.",
                            ));
                        }
                        let project = self.store.register(std::path::Path::new(
                            body["path"]
                                .as_str()
                                .ok_or_else(|| HarnessError::invalid("Invalid project folder."))?,
                        ))?;
                        self.auto_start_graph(project["id"].as_str().unwrap_or(""))
                            .await;
                        Payload::Json(
                            json!({"project":project,"projects":(self.bootstrap)()?["projects"]}),
                        )
                    }
                    "projects.select" => {
                        let project_id = params["project_id"]
                            .as_str()
                            .or_else(|| body["project_id"].as_str())
                            .ok_or_else(|| HarnessError::invalid("Invalid project selection."))?;
                        let project = self.store.project(project_id)?;
                        self.auto_start_graph(project_id).await;
                        Payload::Json(json!({"project":project}))
                    }
                    "graph.status" => {
                        if query["check"].as_str() == Some("1") {
                            self.auto_start_graph(params["project_id"].as_str().unwrap_or(""))
                                .await;
                        }
                        self.domains
                            .get("graph")
                            .ok_or_else(|| {
                                HarnessError::unavailable("Required native service is unavailable.")
                            })?
                            .execute(operation, request.clone())
                            .await?
                    }
                    "sdd.artifacts" => {
                        let session = self.store.get(sid)?;
                        if session["sdd"].is_null() {
                            return Err(HarnessError::invalid("This session does not use SDD."));
                        }
                        let project_path = session["project_path"].as_str().unwrap_or("");
                        let memo_key =
                            build_sdd_artifacts_memo_key(sid, project_path, &session["sdd"]);
                        if let Some(ref key) = memo_key
                            && let Ok(mut guard) = self.runtime_cache.lock()
                            && let Some(cached) = guard.get_sdd_artifacts(key)
                        {
                            Payload::Json(cached)
                        } else {
                            let val = json!({
                                "session_id": sid,
                                "sdd": session["sdd"],
                                "files": crate::sdd::artifacts(
                                    std::path::Path::new(project_path),
                                    &session["sdd"],
                                )?,
                            });
                            if let Some(key) = memo_key
                                && let Ok(mut guard) = self.runtime_cache.lock()
                            {
                                guard.insert_sdd_artifacts(key, val.clone());
                            }
                            Payload::Json(val)
                        }
                    }
                    "omnigraph.branch_act" => {
                        let domain = self.domains.get("omnigraph").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let mut payload = domain.execute(operation, request.clone()).await?;
                        if let Payload::Json(ref mut val) = payload
                            && let Some(updated_session) =
                                crate::sessions::sync_omnigraph_merge_to_session(
                                    &self.store,
                                    &request["body"],
                                    val,
                                )
                        {
                            val["session"] = updated_session;
                        }
                        payload
                    }
                    "omnigraph.share" => {
                        let domain = self.domains.get("omnigraph").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let mut payload = domain.execute(operation, request.clone()).await?;
                        if let Payload::Json(ref mut val) = payload
                            && let Some(updated_session) =
                                crate::sessions::sync_omnigraph_share_to_session(
                                    &self.store,
                                    domain.as_ref(),
                                    &request["body"],
                                    val,
                                )
                                .await
                        {
                            val["session"] = updated_session;
                        }
                        payload
                    }
                    "omnigraph.import" => {
                        let domain = self.domains.get("omnigraph").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let mut payload = domain.execute(operation, request.clone()).await?;
                        if let Payload::Json(ref mut val) = payload {
                            let sessions_domain = self.domains.get("sessions").map(Arc::as_ref);
                            let synced = crate::sessions::sync_omnigraph_import_to_sessions(
                                &self.store,
                                sessions_domain,
                                &request["body"],
                                val,
                            )
                            .await;
                            if !synced.inbox_notifications.is_empty() {
                                val["inbox_notifications"] =
                                    Value::Array(synced.inbox_notifications);
                            }
                            let updated_sessions = synced.sessions;
                            if !updated_sessions.is_empty() {
                                val["session"] = updated_sessions[0].clone();
                                val["sessions"] = Value::Array(updated_sessions);
                            }
                        }
                        payload
                    }
                    "omnigraph.link_sessions" => {
                        let domain = self.domains.get("omnigraph").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let mut payload = domain.execute(operation, request.clone()).await?;
                        if let Payload::Json(ref mut val) = payload {
                            let sessions_domain = self.domains.get("sessions").map(Arc::as_ref);
                            let synced = crate::sessions::sync_omnigraph_link_to_sessions(
                                &self.store,
                                sessions_domain,
                                &request["body"],
                                val,
                            )
                            .await;
                            if !synced.inbox_notifications.is_empty() {
                                val["inbox_notifications"] =
                                    Value::Array(synced.inbox_notifications);
                            }
                            let updated_sessions = synced.sessions;
                            if let Some(first) = updated_sessions.first() {
                                val["session"] = first.clone();
                            }
                            val["sessions"] = Value::Array(updated_sessions);
                        }
                        payload
                    }
                    "knowledge.preflight" => Box::pin(self.handle_preflight(&request)).await?,
                    "knowledge.memory_links" | "knowledge.memory_links_batch" => {
                        Box::pin(self.handle_memory_links(operation, &request)).await?
                    }
                    "delivery.get"
                    | "delivery.act"
                    | "delivery.preview_commit"
                    | "delivery.preview_transfer" => {
                        Box::pin(self.handle_delivery(operation, request.clone())).await?
                    }
                    "omnigraph.sync_knowledge" => {
                        let project_id = body["project_id"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                params["project_id"]
                                    .as_str()
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                            })
                            .ok_or_else(|| HarnessError::invalid("Missing project_id."))?;
                        let knowledge = self.domains.get("knowledge").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let omnigraph = self.domains.get("omnigraph").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let collected =
                            collect_sync_items(knowledge.as_ref(), project_id, query, body).await?;
                        let Payload::Json(synced) = omnigraph
                            .execute(
                                "omnigraph.sync_items",
                                json!({
                                    "params": {},
                                    "query": {},
                                    "body": {
                                        "project_id": project_id,
                                        "items": collected["items"],
                                        "prune_layers": collected["prune_layers"],
                                    },
                                }),
                            )
                            .await?
                        else {
                            return Err(HarnessError::unavailable(
                                "Unexpected omnigraph sync payload.",
                            ));
                        };
                        let ok = synced["ok"] == true;
                        let skip_reason = (!ok)
                            .then(|| synced["skipped"].as_str().map(str::to_owned))
                            .flatten();
                        let mut out = json!({
                            "ok": ok,
                            "branch": synced["branch"].as_str().unwrap_or("knowledge/canonical"),
                            "synced": synced["synced"].as_u64().unwrap_or(0),
                            "brain_count": collected["brain_count"].as_u64().unwrap_or(0),
                            "memory_count": collected["memory_count"].as_u64().unwrap_or(0),
                            "skipped": skip_reason.as_deref().map(|r| json!(r)).unwrap_or_else(|| json!(collected["skipped"].as_u64().unwrap_or(0))),
                            "atom_ids": synced["atom_ids"].as_array().cloned().unwrap_or_default(),
                            "deleted": synced["deleted"].as_u64().unwrap_or(0),
                            "deleted_atom_ids": synced["deleted_atom_ids"].as_array().cloned().unwrap_or_default(),
                            "symbols": synced["symbols"].as_u64().unwrap_or(0),
                        });
                        if let Some(mutated) = synced["mutated_atoms"].as_u64() {
                            out["mutated_atoms"] = json!(mutated);
                        }
                        if let Some(skipped_atoms) = synced["skipped_atoms"].as_u64() {
                            out["skipped_atoms"] = json!(skipped_atoms);
                        }
                        if let Some(skipped_files) = synced["skipped_files"].as_u64() {
                            out["skipped_files"] = json!(skipped_files);
                        }
                        if let Some(error) = synced["prune_error"].as_str() {
                            out["prune_error"] = json!(error);
                        }
                        if !ok {
                            let err_msg = synced["error"]
                                .as_str()
                                .or_else(|| synced["message"].as_str())
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    if skip_reason.as_deref() == Some("omnigraph-off") {
                                        "Omnigraph is off. Configure or enable Omnigraph before syncing.".to_owned()
                                    } else if let Some(reason) = &skip_reason {
                                        format!("Omnigraph sync skipped: {reason}")
                                    } else {
                                        "Omnigraph sync failed.".to_owned()
                                    }
                                });
                            out["error"] = json!(err_msg);
                            out["message"] = json!(err_msg);
                        }
                        Payload::Json(out)
                    }
                    "omnigraph.promote" => {
                        let project_id = body["project_id"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                params["project_id"]
                                    .as_str()
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                            })
                            .ok_or_else(|| HarnessError::invalid("Missing project_id."))?;
                        let atom_id = body["atom_id"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| HarnessError::invalid("Missing atom_id."))?;
                        let knowledge = self.domains.get("knowledge").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let omnigraph = self.domains.get("omnigraph").ok_or_else(|| {
                            HarnessError::unavailable("Required native service is unavailable.")
                        })?;
                        let Payload::Json(fetched) = omnigraph
                            .execute(
                                "omnigraph.fetch_atom",
                                json!({
                                    "params": {},
                                    "query": {},
                                    "body": {
                                        "project_id": project_id,
                                        "atom_id": atom_id,
                                        "branch": body.get("branch").cloned().unwrap_or(Value::Null),
                                    },
                                }),
                            )
                            .await?
                        else {
                            return Err(HarnessError::unavailable(
                                "Unexpected omnigraph atom payload.",
                            ));
                        };
                        let Payload::Json(promoted) = knowledge
                            .execute(
                                "knowledge.promote_atom",
                                json!({
                                    "params": { "project_id": project_id },
                                    "query": {},
                                    "body": {
                                        "project_id": project_id,
                                        "bank": body.get("bank").cloned().unwrap_or(Value::Null),
                                        "session_id": body.get("session_id").cloned().unwrap_or(Value::Null),
                                        "workspace": body.get("workspace").cloned().unwrap_or(Value::Null),
                                        "atom": fetched["atom"],
                                        "files": fetched["files"],
                                        "kind": body.get("kind").cloned().unwrap_or(Value::Null),
                                        "privacy": body.get("privacy").cloned().unwrap_or(Value::Null),
                                    },
                                }),
                            )
                            .await?
                        else {
                            return Err(HarnessError::unavailable(
                                "Unexpected knowledge promote payload.",
                            ));
                        };
                        let branch = fetched["branch"].as_str().unwrap_or("main");
                        let brain_record_id = promoted["record_id"].as_str().unwrap_or("");
                        let revision = promoted["revision"].as_i64().unwrap_or(1);
                        let privacy = promoted
                            .get("privacy")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let mut link_body = json!({
                            "project_id": project_id,
                            "branch": branch,
                            "atom": fetched["atom"],
                            "brain_record_id": brain_record_id,
                            "revision": revision,
                        });
                        if let Some(p) = privacy {
                            link_body["privacy"] = json!(p);
                        }
                        let Payload::Json(linked) = omnigraph
                            .execute(
                                "omnigraph.link_brain",
                                json!({
                                    "params": {},
                                    "query": {},
                                    "body": link_body,
                                }),
                            )
                            .await?
                        else {
                            return Err(HarnessError::unavailable(
                                "Unexpected omnigraph link payload.",
                            ));
                        };
                        let linked_atom = if linked["atom"].is_object() {
                            linked["atom"].clone()
                        } else if linked["atom_id"].is_string() {
                            linked
                        } else {
                            let mut patched = fetched["atom"].clone();
                            if let Some(obj) = patched.as_object_mut() {
                                obj.insert("brain_record_id".into(), json!(brain_record_id));
                                obj.insert("record_revision".into(), json!(revision));
                                obj.insert("origin_layer".into(), json!("project-brain"));
                                obj.insert("authority".into(), json!("verified"));
                                obj.insert("verified".into(), json!(true));
                                if let Some(p) = privacy {
                                    obj.insert("privacy".into(), json!(p));
                                }
                            }
                            patched
                        };
                        Payload::Json(json!({
                            "ok": true,
                            "atom_id": atom_id,
                            "branch": branch,
                            "brain_record_id": brain_record_id,
                            "record_type": promoted["record_type"],
                            "revision": revision,
                            "privacy": promoted["privacy"],
                            "origin_layer": "project-brain",
                            "authority": "verified",
                            "verified_files": promoted["verified_files"],
                            "atom": linked_atom,
                        }))
                    }
                    _ => {
                        let (group, _) = operation.split_once('.').ok_or_else(|| {
                            HarnessError::missing("Unknown native service operation.")
                        })?;
                        self.domains
                            .get(group)
                            .ok_or_else(|| {
                                HarnessError::unavailable("Required native service is unavailable.")
                            })?
                            .execute(operation, request.clone())
                            .await?
                    }
                }
            };
            if let Payload::Json(ref val) = payload
                && val.get("ok").and_then(Value::as_bool) != Some(false)
            {
                let mut_pid = self
                    .resolve_project_id(&[
                        val.get("project_id"),
                        val.get("session").and_then(|s| s.get("project_id")),
                        params.get("project_id"),
                        query.get("project_id"),
                        body.get("project_id"),
                    ])
                    .or_else(|| {
                        (!sid.is_empty())
                            .then(|| self.store.session_project_id(sid))
                            .flatten()
                    });
                if let Some(ref pid) = mut_pid
                    && let Ok(mut guard) = self.runtime_cache.lock()
                {
                    if operation.starts_with("omnigraph.")
                        && !matches!(operation, "omnigraph.status" | "omnigraph.file_context")
                    {
                        guard.bump_omnigraph(pid);
                    } else if operation == "knowledge.run"
                        || operation.starts_with("brain.")
                        || operation.starts_with("memory.")
                        || operation.starts_with("accelerators.")
                    {
                        guard.bump_knowledge(pid);
                    } else if operation.starts_with("graph.")
                        && !matches!(
                            operation,
                            "graph.status" | "graph.live" | "graph.list" | "graph.changes"
                        )
                        && !(operation == "graph.act" && body["action"].as_str() == Some("changes"))
                    {
                        guard.bump_graph(pid);
                    } else if operation.starts_with("sessions.")
                        && !matches!(
                            operation,
                            "sessions.list"
                                | "sessions.get"
                                | "sessions.events"
                                | "sessions.tail"
                                | "sessions.waiting_counts"
                                | "sessions.waiting_permissions"
                                | "sessions.skillcache_preview"
                        )
                    {
                        guard.bump_session(pid);
                    }
                }
            }
            if let Payload::Json(ref val) = payload
                && val["ok"] == true
            {
                if operation == "omnigraph.promote" {
                    let project_id = body["project_id"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            params["project_id"]
                                .as_str()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or("");
                    self.best_effort_sync_knowledge(
                        project_id,
                        body["bank"].as_str(),
                        body["session_id"].as_str(),
                    )
                    .await;
                } else if operation == "knowledge.run"
                    && (matches!(
                        body["action"].as_str(),
                        Some(
                            "complete"
                                | "promote-apply"
                                | "promote-auto"
                                | "bank-retire"
                                | "brain-update"
                                | "record-result"
                                | "bank-reverify"
                        )
                    ) || (body["action"].as_str() == Some("brain-create")
                        && (val["result"]["authority"] == "verified"
                            || (val["result"].is_null() && body["authority"] == "verified"))))
                {
                    self.best_effort_sync_knowledge(
                        params["project_id"].as_str().unwrap_or(""),
                        body["bank"].as_str(),
                        None,
                    )
                    .await;
                } else if matches!(operation, "brain.act" | "memory.save")
                    && !sid.is_empty()
                    && let Ok(session) = self.store.get(sid)
                    && session["omnigraph"]["enabled"] == true
                    && let Some(project_id) = session["project_id"].as_str()
                {
                    self.best_effort_sync_knowledge(
                        project_id,
                        session["brain"]["bank"].as_str(),
                        Some(sid),
                    )
                    .await;
                }
            }
            let bridged_graph_act = operation == "graph.act"
                && !self.overrides.contains_key("graph.act")
                && self
                    .domains
                    .get("graph")
                    .is_some_and(|domain| domain.emits_graph_status_events());
            if !bridged_graph_act {
                self.emit_domain_mutation_sse_with_query(operation, params, query, body, &payload);
            }
            Ok(payload)
        })
    }
}

#[cfg(test)]
mod sync_page_tests {
    use super::*;

    /// A knowledge domain serving `pages` in turn, by the requested offset.
    struct Pages(Vec<Value>, std::sync::Mutex<Vec<u64>>);
    impl Domain for Pages {
        fn execute<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a> {
            Box::pin(async move {
                assert_eq!(operation, "knowledge.sync_collect");
                let offset = request["body"]["offset"].as_u64().unwrap();
                self.1.lock().unwrap().push(offset);
                assert_eq!(request["body"]["bank"], "team");
                Ok(Payload::Json(
                    self.0
                        .iter()
                        .find(|page| page["offset"] == offset)
                        .cloned()
                        .unwrap(),
                ))
            })
        }
    }
    fn page(offset: u64, ids: &[&str], total: u64, next: Option<u64>, layers: &[&str]) -> Value {
        json!({"items": ids.iter().map(|id| json!({"atom_id": id})).collect::<Vec<_>>(), "offset": offset, "total_items": total, "has_more": next.is_some(), "next_offset": next, "complete_layers": layers, "brain_count": 2, "memory_count": 1, "skipped": 4})
    }

    #[tokio::test]
    async fn collects_every_page_and_prunes_only_whole_consistent_layers() {
        let body = json!({"project_id": "p", "bank": "team"});
        let both = ["project-brain", "memory-bank"];
        let whole = Pages(
            vec![
                page(0, &["brain-a", "brain-b"], 3, Some(2), &both),
                page(2, &["mem-c"], 3, None, &both),
            ],
            Default::default(),
        );
        let collected = collect_sync_items(&whole, "p", &json!({}), &body)
            .await
            .unwrap();
        assert_eq!(*whole.1.lock().unwrap(), vec![0, 2]);
        assert_eq!(
            collected["items"],
            json!([{"atom_id":"brain-a"},{"atom_id":"brain-b"},{"atom_id":"mem-c"}])
        );
        assert_eq!(
            collected["prune_layers"],
            json!(["memory-bank", "project-brain"])
        );
        assert_eq!(collected["brain_count"], 2);
        // A layer one page does not report whole is not pruned.
        let partial = Pages(
            vec![
                page(0, &["brain-a"], 2, Some(1), &both),
                page(1, &["mem-c"], 2, None, &["memory-bank"]),
            ],
            Default::default(),
        );
        let collected = collect_sync_items(&partial, "p", &json!({}), &body)
            .await
            .unwrap();
        assert_eq!(collected["prune_layers"], json!(["memory-bank"]));
        // Knowledge that changed between pages prunes nothing.
        let moved = Pages(
            vec![
                page(0, &["brain-a"], 2, Some(1), &both),
                page(1, &["brain-b", "mem-c"], 3, None, &both),
            ],
            Default::default(),
        );
        let collected = collect_sync_items(&moved, "p", &json!({}), &body)
            .await
            .unwrap();
        assert_eq!(collected["prune_layers"], json!([]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct StubDomain;
    impl Domain for StubDomain {
        fn execute<'a>(&'a self, _operation: &'a str, _request: Value) -> Reply<'a> {
            Box::pin(async { Ok(Payload::Json(json!({}))) })
        }
    }


    #[derive(Clone, Default)]
    struct RecordingDomain {
        calls: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
        promote_error: bool,
        sync_off: bool,
    }
    impl Domain for RecordingDomain {
        fn execute<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a> {
            let op = operation.to_owned();
            let calls = self.calls.clone();
            let promote_error = self.promote_error;
            let sync_off = self.sync_off;
            Box::pin(async move {
                calls.lock().unwrap().push((op.clone(), request.clone()));
                let val = match op.as_str() {
                    "knowledge.sync_collect" => json!({
                        "items": [
                            {"atom_id": "brain-b1", "kind": "finding", "title": "Brain rule"},
                            {"atom_id": "mem-MEM-0001", "kind": "insight", "title": "Bank chunk"}
                        ],
                        "brain_count": 1,
                        "memory_count": 1,
                        "skipped": 2
                    }),
                    "omnigraph.status" => {
                        if sync_off {
                            json!({"state": "unconfigured", "mode": "off"})
                        } else {
                            json!({"state": "connected", "mode": "external"})
                        }
                    }
                    "omnigraph.sync_items" => {
                        if sync_off {
                            json!({
                                "ok": false,
                                "skipped": "omnigraph-off",
                                "branch": "knowledge/canonical",
                                "synced": 0,
                                "atom_ids": []
                            })
                        } else {
                            json!({
                                "ok": true,
                                "branch": "knowledge/canonical",
                                "synced": 2,
                                "atom_ids": ["brain-b1", "mem-MEM-0001"]
                            })
                        }
                    }
                    "omnigraph.fetch_atom" => json!({
                        "branch": "session/s1",
                        "files": ["src/auth.rs"],
                        "atom": {
                            "atom_id": "atom-1",
                            "kind": "finding",
                            "title": "Validate token",
                            "summary": "Check exp claim",
                            "files": ["src/auth.rs"]
                        }
                    }),
                    "knowledge.promote_atom" => {
                        if promote_error {
                            return Err(HarnessError::conflict("Cited file missing."));
                        }
                        json!({
                            "record_id": "11111111-1111-4111-8111-111111111111",
                            "record_type": "finding",
                            "revision": 1,
                            "verified_files": ["src/auth.rs"],
                            "privacy": "team",
                            "authority": "verified",
                            "origin_layer": "project-brain"
                        })
                    }
                    "omnigraph.link_brain" => json!({
                        "atom": {
                            "atom_id": "atom-1",
                            "brain_record_id": "11111111-1111-4111-8111-111111111111",
                            "record_revision": 1,
                            "origin_layer": "project-brain",
                            "authority": "verified",
                            "verified": true
                        }
                    }),
                    "knowledge.run" | "brain.act" | "memory.save" => json!({"ok": true}),
                    _ => json!({}),
                };
                Ok(Payload::Json(val))
            })
        }
    }

    fn build_services_with_recording(
        store: SessionStore,
        rec: Arc<RecordingDomain>,
        override_memory_save: bool,
    ) -> NativeServices {
        let stub: Arc<dyn Domain> = Arc::new(StubDomain);
        let mut domains: BTreeMap<String, Arc<dyn Domain>> = [
            "sessions",
            "systems",
            "discovery",
            "results",
            "delivery",
            "projects",
            "commands",
            "files",
            "providers",
            "omnigraph",
            "working_memory",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), Arc::clone(&stub)))
        .collect();
        domains.insert("knowledge".into(), rec.clone());
        domains.insert("omnigraph".into(), rec.clone());
        domains.insert("brain".into(), rec.clone());
        let mut overrides = BTreeMap::new();
        if override_memory_save {
            overrides.insert("memory.save".into(), rec as Arc<dyn Domain>);
        }
        NativeServices::new(
            store,
            domains,
            Arc::new(|| Ok(json!({}))),
            overrides,
            Arc::new(|| Box::pin(async { Ok(()) })),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn omnigraph_sync_knowledge_promote_and_post_sync_wire_domains() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&root.path().join("state")).unwrap();
        let project_dir = root.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = store.register(&project_dir).unwrap();
        let pid = project["id"].as_str().unwrap().to_owned();
        let sess = store
            .create(
                &json!({
                    "project_id": pid,
                    "provider": "claude",
                    "mode": "edit",
                    "workflow": "native",
                    "project_context": false,
                    "omnigraph": {"enabled": true}
                }),
                "Sync test",
                &[],
            )
            .unwrap();
        let sid = sess["id"].as_str().unwrap().to_owned();

        let rec = Arc::new(RecordingDomain::default());
        let services = build_services_with_recording(store.clone(), rec.clone(), true);

        // 1. omnigraph.sync_knowledge
        let Payload::Json(sync_res) = services
            .request(
                "omnigraph.sync_knowledge",
                json!({"params":{},"query":{},"body":{"project_id": pid}}),
            )
            .await
            .unwrap()
        else {
            panic!("expected json");
        };
        assert_eq!(sync_res["ok"], true);
        assert_eq!(sync_res["branch"], "knowledge/canonical");
        assert_eq!(sync_res["synced"], 2);
        assert_eq!(sync_res["brain_count"], 1);
        assert_eq!(sync_res["memory_count"], 1);
        assert_eq!(sync_res["skipped"], 2);
        assert_eq!(sync_res["atom_ids"], json!(["brain-b1", "mem-MEM-0001"]));

        // 2. omnigraph.promote happy path
        let Payload::Json(promote_res) = services
            .request(
                "omnigraph.promote",
                json!({"params":{},"query":{},"body":{"project_id": pid, "atom_id": "atom-1"}}),
            )
            .await
            .unwrap()
        else {
            panic!("expected json");
        };
        assert_eq!(promote_res["ok"], true);
        assert_eq!(
            promote_res["brain_record_id"],
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(promote_res["record_type"], "finding");
        assert_eq!(promote_res["revision"], 1);
        assert_eq!(promote_res["origin_layer"], "project-brain");
        assert_eq!(promote_res["authority"], "verified");
        assert_eq!(promote_res["verified_files"], json!(["src/auth.rs"]));
        assert_eq!(promote_res["privacy"], "team");
        assert_eq!(
            promote_res["atom"]["brain_record_id"],
            "11111111-1111-4111-8111-111111111111"
        );
        {
            let recorded = rec.calls.lock().unwrap();
            let link_call = recorded
                .iter()
                .find(|(op, _)| op == "omnigraph.link_brain")
                .expect("link_brain called");
            assert_eq!(link_call.1["body"]["privacy"], "team");
            assert!(
                recorded.iter().any(|(op, _)| op == "omnigraph.sync_items"),
                "omnigraph.promote must trigger canonical sync"
            );
        }

        // 3. Best-effort sync triggers after knowledge.run(complete|brain-create verified|bank-reverify), brain.act, and memory.save
        rec.calls.lock().unwrap().clear();
        services
            .request(
                "knowledge.run",
                json!({"params":{"project_id": pid},"query":{},"body":{"action":"status"}}),
            )
            .await
            .unwrap();
        assert_eq!(
            rec.calls.lock().unwrap().len(),
            1,
            "status must not trigger sync"
        );

        rec.calls.lock().unwrap().clear();
        services
            .request(
                "knowledge.run",
                json!({"params":{"project_id": pid},"query":{},"body":{"action":"brain-create","authority":"observed"}}),
            )
            .await
            .unwrap();
        assert_eq!(
            rec.calls.lock().unwrap().len(),
            1,
            "unverified brain-create must not trigger sync"
        );

        for body in [
            json!({"action":"complete"}),
            json!({"action":"brain-create","authority":"verified"}),
            json!({"action":"bank-reverify"}),
        ] {
            rec.calls.lock().unwrap().clear();
            services
                .request(
                    "knowledge.run",
                    json!({"params":{"project_id": pid},"query":{},"body": body}),
                )
                .await
                .unwrap();
            let ops: Vec<String> = rec
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|(op, _)| op.clone())
                .collect();
            assert!(
                ops.contains(&"knowledge.sync_collect".to_string()),
                "{ops:?}"
            );
            assert!(ops.contains(&"omnigraph.sync_items".to_string()), "{ops:?}");
        }

        for op in ["brain.act", "memory.save"] {
            rec.calls.lock().unwrap().clear();
            services
                .request(
                    op,
                    json!({"params":{"session_id": sid},"query":{},"body":{"action":"brain-update"}}),
                )
                .await
                .unwrap();
            let ops: Vec<String> = rec
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|(op, _)| op.clone())
                .collect();
            assert!(
                ops.contains(&"knowledge.sync_collect".to_string()),
                "{op}: {ops:?}"
            );
            assert!(
                ops.contains(&"omnigraph.sync_items".to_string()),
                "{op}: {ops:?}"
            );
        }

        // 4. omnigraph.sync_knowledge preserves omnigraph-off skip reason and error message
        let off_rec = Arc::new(RecordingDomain {
            sync_off: true,
            ..Default::default()
        });
        let off_services = build_services_with_recording(store.clone(), off_rec, false);
        let Payload::Json(off_res) = off_services
            .request(
                "omnigraph.sync_knowledge",
                json!({"params":{},"query":{},"body":{"project_id": pid}}),
            )
            .await
            .unwrap()
        else {
            panic!("expected json");
        };
        assert_eq!(off_res["ok"], false);
        assert_eq!(off_res["skipped"], "omnigraph-off");
        assert!(
            off_res["error"]
                .as_str()
                .is_some_and(|s| s.contains("Omnigraph is off")),
            "{off_res}"
        );
        assert_eq!(off_res["message"], off_res["error"]);

        // 5. Promote error aborts before omnigraph.link_brain
        let failing_rec = Arc::new(RecordingDomain {
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            promote_error: true,
            sync_off: false,
        });
        let failing_services = build_services_with_recording(store, failing_rec.clone(), false);
        let err = failing_services
            .request(
                "omnigraph.promote",
                json!({"params":{},"query":{},"body":{"project_id": pid, "atom_id": "atom-1"}}),
            )
            .await
            .err()
            .expect("promote error");
        assert_eq!(err.status, 409);
        let ops: Vec<String> = failing_rec
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(op, _)| op.clone())
            .collect();
        assert!(
            !ops.contains(&"omnigraph.link_brain".to_string()),
            "{ops:?}"
        );

        services.close().await.unwrap();
        off_services.close().await.unwrap();
        failing_services.close().await.unwrap();
    }

    struct WaitingAndBatchDomain;
    impl Domain for WaitingAndBatchDomain {
        fn execute<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a> {
            let op = operation.to_owned();
            Box::pin(async move {
                let val = match op.as_str() {
                    "sessions.waiting_counts" => json!({"counts": {"s1": 2}}),
                    "sessions.waiting_permissions" => {
                        let sid = request["params"]["session_id"].as_str().unwrap_or("");
                        if sid == "s1" {
                            json!({"items": [{"kind":"permission","state":"pending","request_id":"req-1"}]})
                        } else {
                            json!({"items": []})
                        }
                    }
                    "knowledge.memory_links" => {
                        let path = request["query"]["path"].as_str().unwrap_or("");
                        json!({
                            "project_id": request["params"]["project_id"],
                            "path": path,
                            "brain_records": [],
                            "memory_chunks": [],
                            "hygiene_warnings": [],
                            "contradiction_warnings": [],
                        })
                    }
                    "knowledge.memory_links_batch" => json!({
                        "project_id": request["params"]["project_id"],
                        "items": [
                            {
                                "project_id": request["params"]["project_id"],
                                "path": "src/ok.rs",
                                "brain_records": [],
                                "memory_chunks": [],
                                "hygiene_warnings": [],
                                "contradiction_warnings": [],
                            },
                            {
                                "project_id": request["params"]["project_id"],
                                "path": "src/skip.rs",
                                "brain_records": [],
                                "memory_chunks": [],
                                "hygiene_warnings": [],
                                "contradiction_warnings": [],
                            },
                            {
                                "project_id": request["params"]["project_id"],
                                "path": "src/err.rs",
                                "brain_records": [],
                                "memory_chunks": [],
                                "hygiene_warnings": [],
                                "contradiction_warnings": [],
                            },
                        ]
                    }),
                    "omnigraph.file_context" => {
                        match request["query"]["path"].as_str().unwrap_or("") {
                            "src/ok.rs" => json!({
                                "ok": true,
                                "atoms": [
                                    {"atom_id": "brain-a1", "title": "Claim A", "brain_record_id": "rec-a"},
                                    {"atom_id": "mem-MEM-1", "title": "Claim B", "memory_id": "MEM-1"}
                                ],
                                "contradictions": [{
                                    "source_id": "brain-a1",
                                    "source_title": "Claim A",
                                    "target_id": "mem-MEM-1",
                                    "target_title": "Claim B",
                                }],
                            }),
                            "src/skip.rs" => json!({"ok": false, "skipped": "omnigraph-off"}),
                            "src/err.rs" => {
                                return Err(HarnessError::unavailable("Omnigraph offline."));
                            }
                            _ => json!({"ok": true, "atoms": [], "contradictions": []}),
                        }
                    }
                    _ => json!({}),
                };
                Ok(Payload::Json(val))
            })
        }
    }


    struct PreflightAndDeliveryDomain {
        fail_knowledge: bool,
        fail_omnigraph: bool,
    }
    impl Domain for PreflightAndDeliveryDomain {
        fn execute<'a>(&'a self, operation: &'a str, request: Value) -> Reply<'a> {
            let op = operation.to_owned();
            let fail_knowledge = self.fail_knowledge;
            let fail_omnigraph = self.fail_omnigraph;
            Box::pin(async move {
                if op == "knowledge.preflight" {
                    if fail_knowledge {
                        return Err(HarnessError::unavailable("Knowledge offline."));
                    }
                    let pid = request["params"]["project_id"].clone();
                    return Ok(Payload::Json(json!({
                        "project_id": pid,
                        "files": [
                            {
                                "path": "src/ok.rs",
                                "brain_records": [],
                                "memory_chunks": [],
                                "hygiene_warnings": [],
                                "contradiction_warnings": [],
                            },
                            {
                                "path": "src/missing.rs",
                                "brain_records": [],
                                "memory_chunks": [],
                                "hygiene_warnings": [{
                                    "kind": "cited-file-missing",
                                    "path": "src/missing.rs",
                                    "linked": ["rec-m"],
                                    "recommended_action": "review",
                                    "text": "Missing file",
                                }],
                                "contradiction_warnings": [],
                            },
                            {
                                "path": "src/err.rs",
                                "brain_records": [],
                                "memory_chunks": [],
                                "hygiene_warnings": [],
                                "contradiction_warnings": [],
                            },
                        ],
                        "contradiction_warnings": [],
                        "hygiene_warnings": [{
                            "kind": "cited-file-missing",
                            "path": "src/missing.rs",
                            "linked": ["rec-m"],
                            "recommended_action": "review",
                            "text": "Missing file",
                        }],
                        "risk_callers": [{
                            "from": "src/caller.rs",
                            "to": "src/ok.rs",
                            "impact": "impacted",
                        }],
                        "blast_summary": {
                            "direct_count": 1,
                            "downstream_count": 0,
                            "files": [{"path": "src/caller.rs", "hop": 1, "left": 0.85}],
                        },
                        "risk_caller_hints": [],
                    })));
                }
                if op == "omnigraph.file_context" {
                    if fail_omnigraph {
                        return Err(HarnessError::unavailable("Omnigraph offline."));
                    }
                    return match request["query"]["path"].as_str().unwrap_or("") {
                        "src/ok.rs" => Ok(Payload::Json(json!({
                            "ok": true,
                            "atoms": [
                                {"atom_id": "brain-a1", "title": "Claim A", "brain_record_id": "rec-a"},
                                {"atom_id": "mem-MEM-1", "title": "Claim B", "memory_id": "MEM-1"}
                            ],
                            "contradictions": [{
                                "source_id": "brain-a1",
                                "source_title": "Claim A",
                                "target_id": "mem-MEM-1",
                                "target_title": "Claim B",
                            }],
                        }))),
                        "src/err.rs" => Err(HarnessError::unavailable("Omnigraph file timeout.")),
                        _ => Ok(Payload::Json(
                            json!({"ok": true, "atoms": [], "contradictions": []}),
                        )),
                    };
                }
                if op == "delivery.preview_commit"
                    || (op == "delivery.act" && request["body"]["action"] == "preview_commit")
                {
                    return Ok(Payload::Json(json!({
                        "kind": "commit",
                        "can_apply": true,
                        "files": [
                            {"status": "M", "path": "src/ok.rs"},
                            {"status": "D", "path": "src/missing.rs"},
                        ],
                        "preview_id": "p-commit",
                    })));
                }
                if op == "delivery.preview_transfer"
                    || (op == "delivery.act" && request["body"]["action"] == "preview_transfer")
                {
                    return Ok(Payload::Json(json!({
                        "kind": "transfer",
                        "can_apply": true,
                        "files": [{"status": "M", "path": "src/ok.rs"}],
                        "preview_id": "p-transfer",
                    })));
                }
                if op == "delivery.get" {
                    return Ok(Payload::Json(json!({
                        "available": true,
                        "preview": {
                            "kind": "transfer",
                            "can_apply": true,
                            "files": [{"status": "M", "path": "src/ok.rs"}],
                            "preview_id": "p-current",
                        },
                    })));
                }
                Ok(Payload::Json(json!({})))
            })
        }
    }





    fn set_dir_modified_for_test(dir: &Path, time: SystemTime) {
        let mut opts = std::fs::File::options();
        #[cfg(unix)]
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            opts.access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        opts.open(dir).unwrap().set_modified(time).unwrap();
    }


    #[tokio::test]
    async fn best_effort_sync_knowledge_short_circuits_when_settled_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&dir.path().join("state")).unwrap();
        let proj = dir.path().join("proj-sync");
        let bank_dir = proj.join("memory-bank");
        std::fs::create_dir_all(&bank_dir).unwrap();
        let mem_file = bank_dir.join("MEM-20260101-demo.md");
        std::fs::write(&mem_file, "# Demo\nSettled chunk.\n").unwrap();
        let settled_time = SystemTime::now() - std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&mem_file)
            .unwrap()
            .set_modified(settled_time)
            .unwrap();
        set_dir_modified_for_test(&bank_dir, settled_time);
        let reg = store.register(&proj).unwrap();
        let pid = reg["id"].as_str().unwrap().to_owned();

        let rec = Arc::new(RecordingDomain::default());
        let services = build_services_with_recording(store, rec.clone(), false);

        services.best_effort_sync_knowledge(&pid, None, None).await;
        let ops_after_first: Vec<String> = rec
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(op, _)| op.clone())
            .collect();
        assert_eq!(
            ops_after_first
                .iter()
                .filter(|op| op.as_str() == "knowledge.sync_collect")
                .count(),
            1
        );

        // Unchanged settled revision stamp short-circuits without calling knowledge.sync_collect or omnigraph.sync_items.
        services.best_effort_sync_knowledge(&pid, None, None).await;
        let ops_after_second: Vec<String> = rec
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(op, _)| op.clone())
            .collect();
        assert_eq!(
            ops_after_second
                .iter()
                .filter(|op| op.as_str() == "knowledge.sync_collect")
                .count(),
            1
        );

        // Recent modification inside KNOWLEDGE_RACY_NS invalidates and refuses short-circuit.
        std::fs::write(&mem_file, "# Demo\nFresh edit.\n").unwrap();
        services.best_effort_sync_knowledge(&pid, None, None).await;
        let ops_after_edit: Vec<String> = rec
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(op, _)| op.clone())
            .collect();
        assert_eq!(
            ops_after_edit
                .iter()
                .filter(|op| op.as_str() == "knowledge.sync_collect")
                .count(),
            2
        );
        services.close().await.unwrap();
    }




}
