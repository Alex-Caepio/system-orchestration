use crate::{
    HarnessError, Result,
    store::{self, SessionStore},
};
use base64::Engine;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};
pub const MAX_JSON_BYTES: usize = 12 * 1024 * 1024;
/// Bytes an attachment filename may have.
pub const NAME_BYTES: usize = 200;
pub struct Upload {
    pub name: String,
    pub bytes: Vec<u8>,
}
pub fn validate(value: &Value) -> Result<Vec<Upload>> {
    let items = value
        .as_array()
        .ok_or_else(|| HarnessError::invalid("Attachments must be a list."))?;
    if items.len() > 5 {
        return Err(HarnessError::invalid("Attach up to 5 files per message."));
    }
    let mut size = 0;
    let mut out = Vec::new();
    for item in items {
        if !store::exact(item, &["name", "data"], &[]) {
            return Err(HarnessError::invalid(
                "Each attachment requires name and base64 data.",
            ));
        }
        let name = item["name"]
            .as_str()
            .ok_or_else(|| HarnessError::invalid("Invalid attachment filename."))?;
        if name.is_empty()
            || name.len() > NAME_BYTES
            || name.chars().any(|c| c.is_control() || "/\\:".contains(c))
            || [".", ".."].contains(&name)
            || name.ends_with('.')
            || name.ends_with(' ')
        {
            return Err(HarnessError::invalid(
                "Use an attachment filename without paths or control characters.",
            ));
        }
        let data = item["data"]
            .as_str()
            .ok_or_else(|| HarnessError::invalid("Invalid attachment base64."))?;
        if data.len() > 4 * (4_usize * 1024 * 1024).div_ceil(3) {
            return Err(HarnessError::invalid(
                "Each attachment must be at most 4 MiB.",
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| HarnessError::invalid("Invalid attachment base64."))?;
        size += bytes.len();
        if bytes.len() > 4 * 1024 * 1024 || size > 8 * 1024 * 1024 {
            return Err(HarnessError::invalid(
                "Attachments must be at most 4 MiB each and 8 MiB per message.",
            ));
        }
        out.push(Upload {
            name: name.into(),
            bytes,
        });
    }
    Ok(out)
}
pub fn save(store: &SessionStore, sid: &str, uploads: Vec<Upload>) -> Result<Vec<Value>> {
    let mut db = store.connection()?;
    let tx = db.transaction()?;
    let (saved, mut guard) = stage(&tx, &store.state, sid, &uploads)?;
    tx.commit()?;
    guard.commit();
    Ok(saved)
}
pub fn read(store: &SessionStore, sid: &str, id: &str) -> Result<(Value, Vec<u8>, PathBuf)> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(HarnessError::invalid("Invalid attachment ID."));
    }
    let row = store
        .connection()?
        .query_row(
            "SELECT name,size,digest FROM attachments WHERE id=?1 AND session_id=?2",
            params![id, sid],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| HarnessError::missing("Attachment not found in this session."))?;
    let path = store
        .state
        .join("attachments")
        .join(id)
        .join(format!("upload-{}", row.0));
    orch_core::fs::admit_descendant(&store.state, &path)?;
    let root = store.state.join("attachments");
    let mut files = orch_core::publication::PublicationFiles::new(&root)?;
    let relative = format!("{id}/upload-{}", row.0);
    let bytes = files
        .read(&relative, 4 * 1024 * 1024)?
        .ok_or_else(|| HarnessError::missing("The stored attachment is unavailable."))?;
    if bytes.len() as u64 != row.1 || store::sha(&bytes) != row.2 {
        return Err(HarnessError::conflict(
            "The stored attachment changed. Attach the file again.",
        ));
    }
    Ok((
        json!({"id":id,"name":row.0,"size":row.1,"digest":row.2}),
        bytes,
        path,
    ))
}

/// The folders that hold a session's uploads, oldest first. A prompt lists an
/// upload by its path in one of them, and a provider that reads only inside
/// named folders has to be given these: Claude Code in Edit mode refuses the
/// listed path otherwise. Earlier messages' folders stay in the list because
/// a resumed conversation still names their paths.
pub fn directories(store: &SessionStore, sid: &str) -> Result<Vec<PathBuf>> {
    let root = store.state.join("attachments");
    let ids = {
        let db = store.connection()?;
        let mut query =
            db.prepare("SELECT id FROM attachments WHERE session_id=?1 ORDER BY rowid")?;
        query
            .query_map([sid], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut folders = Vec::new();
    for id in ids {
        if id.len() != 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            continue;
        }
        let folder = root.join(&id);
        orch_core::fs::admit_descendant(&root, &folder)?;
        // An upload whose folder is gone is reported when its message reads it.
        if std::fs::symlink_metadata(&folder).is_ok_and(|metadata| metadata.is_dir()) {
            folders.push(folder);
        }
    }
    Ok(folders)
}

pub struct UploadGuard {
    pub folders: Vec<PathBuf>,
    pub committed: bool,
}
impl UploadGuard {
    pub fn commit(&mut self) {
        self.committed = true;
    }
}
impl Drop for UploadGuard {
    fn drop(&mut self) {
        if !self.committed {
            for folder in &self.folders {
                #[cfg(windows)]
                #[expect(
                    clippy::permissions_set_readonly_false,
                    reason = "Owned upload cleanup clears the Windows ReadOnly attribute; this block is excluded on Unix."
                )]
                {
                    if let Ok(entries) = std::fs::read_dir(folder) {
                        for entry in entries.flatten() {
                            if entry
                                .file_name()
                                .to_str()
                                .is_some_and(|n| n.starts_with("upload-"))
                                && let Ok(metadata) = std::fs::symlink_metadata(entry.path())
                                && metadata.is_file()
                                && !orch_core::fs::linked(&metadata)
                            {
                                let mut mode = metadata.permissions();
                                mode.set_readonly(false);
                                let _ = std::fs::set_permissions(entry.path(), mode);
                            }
                        }
                    }
                }
                let _ = std::fs::remove_dir_all(folder);
            }
        }
    }
}
pub fn stage(
    db: &rusqlite::Connection,
    state: &Path,
    sid: &str,
    uploads: &[Upload],
) -> Result<(Vec<Value>, UploadGuard)> {
    let root = state.join("attachments");
    orch_host::util::private_directory(&root)?;
    let mut guard = UploadGuard {
        folders: Vec::new(),
        committed: false,
    };
    let mut saved = Vec::new();
    for upload in uploads {
        let id = store::hexid();
        let folder = root.join(&id);
        orch_core::fs::admit_descendant(&root, &folder)?;
        std::fs::create_dir(&folder)?;
        orch_host::util::private_directory(&folder)?;
        guard.folders.push(folder.clone());
        let file = folder.join(format!("upload-{}", upload.name));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o400)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut output = options.open(&file)?;
        output.write_all(&upload.bytes)?;
        output.sync_all()?;
        drop(output);
        orch_host::util::private_file(&file)?;
        let mut rooted = orch_core::publication::PublicationFiles::new(&folder)?;
        rooted.set_mode(&format!("upload-{}", upload.name), 0o400)?;
        db.execute(
            "INSERT INTO attachments VALUES(?1,?2,?3,?4,?5)",
            params![
                id,
                sid,
                upload.name,
                upload.bytes.len() as i64,
                store::sha(&upload.bytes)
            ],
        )?;
        saved.push(json!({"id":id,"name":upload.name,"size":upload.bytes.len()}));
    }
    Ok((saved, guard))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_store() -> (tempfile::TempDir, SessionStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&directory.path().join("state")).unwrap();
        store
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO sessions(id,title,project_id,project_path,provider,mode,workflow,project_context,status,created_at,updated_at) VALUES('chat','Example','project','/project','codex','plan','native',0,'completed','created','updated')",
                [],
            )
            .unwrap();
        (directory, store)
    }

    fn uploads(files: &[(&str, &[u8])]) -> Vec<Upload> {
        let encoded: Vec<Value> = files
            .iter()
            .map(|(name, bytes)| {
                json!({"name":name,"data":base64::engine::general_purpose::STANDARD.encode(bytes)})
            })
            .collect();
        validate(&json!(encoded)).unwrap()
    }

    /// Saving takes the path every message attachment takes: an owner-only
    /// copy whose mode `PublicationFiles` pins at 0400. That check once
    /// compared the permission bits it had just changed and refused every
    /// upload on Unix, and no test saved one.
    #[test]
    fn a_saved_upload_is_private_read_only_and_reads_back() {
        let (_directory, store) = session_store();
        let context = "# Context from a task\n\nПочему итог расходится на копейку?\n".as_bytes();
        let files: [(&str, &[u8]); 2] = [
            ("context-почему-итог-cc7155a8.md", context),
            ("notes.txt", b"plain notes"),
        ];
        let saved = save(&store, "chat", uploads(&files)).unwrap();
        assert_eq!(saved.len(), files.len());
        for (item, (name, body)) in saved.iter().zip(files) {
            assert_eq!(item["name"], name);
            assert_eq!(item["size"], body.len());
            let id = item["id"].as_str().unwrap();
            let (stored, bytes, path) = read(&store, "chat", id).unwrap();
            assert_eq!(bytes, body);
            assert_eq!(stored["digest"], store::sha(body));
            assert_eq!(path.file_name().unwrap(), format!("upload-{name}").as_str());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode(&path) & 0o7777, 0o400, "{}", path.display());
                assert_eq!(mode(path.parent().unwrap()) & 0o7777, 0o700);
            }
        }
        let id = saved[0]["id"].as_str().unwrap();
        let other = read(&store, "other-chat", id).unwrap_err();
        assert!(other.message.contains("not found in this session"));
    }

    /// A launch names these folders to a provider that reads only inside named
    /// folders. Earlier messages' uploads stay listed, another session's never.
    #[test]
    fn a_session_lists_the_folders_of_its_own_uploads_oldest_first() {
        let (_directory, store) = session_store();
        store
            .connection()
            .unwrap()
            .execute(
                "INSERT INTO sessions(id,title,project_id,project_path,provider,mode,workflow,project_context,status,created_at,updated_at) VALUES('other-chat','Other','project','/project','claude','edit','native',0,'completed','created','updated')",
                [],
            )
            .unwrap();
        let root = store.state.join("attachments");
        let folder = |item: &Value| root.join(item["id"].as_str().unwrap());
        assert!(directories(&store, "chat").unwrap().is_empty());
        let first = save(&store, "chat", uploads(&[("first.md", b"one")])).unwrap();
        let foreign = save(&store, "other-chat", uploads(&[("foreign.md", b"four")])).unwrap();
        let second = save(
            &store,
            "chat",
            uploads(&[("second.md", b"two"), ("third.md", b"three")]),
        )
        .unwrap();
        let own: Vec<PathBuf> = first.iter().chain(&second).map(folder).collect();
        assert_eq!(directories(&store, "chat").unwrap(), own);
        assert_eq!(
            directories(&store, "other-chat").unwrap(),
            [folder(&foreign[0])]
        );
        for directory in &own {
            assert_eq!(directory.parent().unwrap(), root);
            assert!(directory.is_dir(), "{}", directory.display());
        }
        // A folder removed behind the store's back is left out, not named.
        #[cfg(unix)]
        {
            std::fs::remove_dir_all(&own[0]).unwrap();
            assert_eq!(directories(&store, "chat").unwrap(), own[1..]);
        }
    }

    /// A message that fails after its files were staged leaves none behind.
    #[test]
    fn an_uncommitted_upload_leaves_no_files() {
        let (_directory, store) = session_store();
        let staged = {
            let mut db = store.connection().unwrap();
            let tx = db.transaction().unwrap();
            let (staged, _guard) = stage(
                &tx,
                &store.state,
                "chat",
                &uploads(&[("first.md", b"one"), ("second.md", b"two")]),
            )
            .unwrap();
            staged
        };
        let root = store.state.join("attachments");
        for item in &staged {
            let id = item["id"].as_str().unwrap();
            assert!(!root.join(id).exists(), "{id} survived");
            assert!(read(&store, "chat", id).is_err());
        }
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }
}
