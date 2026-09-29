//! Host-wide coordination journal and immutable artifact snapshots.

use gns_core::*;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug)]
pub struct CoordinationStore {
    conn: Mutex<Connection>,
    root: PathBuf,
}

impl CoordinationStore {
    pub fn open(root: &Path) -> Result<Self, HostError> {
        std::fs::create_dir_all(root.join("artifacts"))?;
        let conn = Connection::open(root.join("coordination.db")).map_err(HostError::storage)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(HostError::storage)?;
        conn.pragma_update(None, "synchronous", "FULL").map_err(HostError::storage)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(HostError::storage)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS artifacts(id TEXT PRIMARY KEY, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS artifact_grants(artifact TEXT NOT NULL, principal TEXT NOT NULL, PRIMARY KEY(artifact,principal));
            CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, request_key TEXT UNIQUE, data TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS task_deliveries(id TEXT PRIMARY KEY, data TEXT NOT NULL);
            PRAGMA user_version=1;",
        )
        .map_err(HostError::storage)?;
        Ok(Self { conn: Mutex::new(conn), root: root.to_path_buf() })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, HostError> {
        self.conn.lock().map_err(|_| HostError::storage("coordination database lock poisoned"))
    }

    /// The caller validates the source against the sending agent's roots before invoking this.
    pub fn snapshot(&self, creator: &AgentId, source: &Path, max_bytes: u64) -> Result<ArtifactRef, HostError> {
        let mut input = File::open(source)?;
        let before = input.metadata()?;
        if !before.is_file() {
            return Err(HostError::invalid("only regular files can be published; archive directories first"));
        }
        if before.len() > max_bytes {
            return Err(HostError::invalid(format!("file exceeds the {max_bytes} byte limit")));
        }
        let id = EntryId::new().to_string();
        let dir = self.root.join("artifacts").join(&id);
        std::fs::create_dir(&dir)?;
        let pending = dir.join("pending");
        let result = (|| {
            let mut output = OpenOptions::new().write(true).create_new(true).open(&pending)?;
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > max_bytes {
                    return Err(HostError::invalid("file grew beyond the artifact size limit"));
                }
                hash.update(&buffer[..n]);
                output.write_all(&buffer[..n])?;
            }
            let after = input.metadata()?;
            if size != before.len() || before.len() != after.len() || before.modified()? != after.modified()? {
                return Err(HostError::invalid("source file changed while being copied; finish writing and retry"));
            }
            output.sync_all()?;
            drop(output);
            std::fs::rename(&pending, dir.join("content"))?;
            // Directory fsync is a Unix operation; opening a directory as a
            // File fails with AccessDenied on Windows. The content was synced
            // above on every platform before committing its metadata.
            #[cfg(unix)]
            File::open(&dir)?.sync_all()?;
            let name =
                source.file_name().and_then(|s| s.to_str()).ok_or_else(|| HostError::invalid("file name must be valid UTF-8"))?.to_owned();
            let artifact = ArtifactRef {
                id: id.clone(),
                name,
                media_type: mime_guess::from_path(source).first_or_octet_stream().to_string(),
                size,
                sha256: format!("{:x}", hash.finalize()),
                creator: creator.clone(),
                created_at: gns_core::text::now_ms(),
            };
            let mut conn = self.lock()?;
            let tx = conn.transaction().map_err(HostError::storage)?;
            tx.execute("INSERT INTO artifacts(id,data) VALUES (?,?)", params![id, serde_json::to_string(&artifact)?])
                .map_err(HostError::storage)?;
            tx.execute(
                "INSERT INTO artifact_grants(artifact,principal) VALUES (?,?)",
                params![id, ConversationRef::Agent(creator.clone()).grant_key()],
            )
            .map_err(HostError::storage)?;
            tx.commit().map_err(HostError::storage)?;
            Ok(artifact)
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        result
    }

    pub fn artifact(&self, id: &str) -> Result<ArtifactRef, HostError> {
        let raw: Option<String> =
            self.lock()?.query_row("SELECT data FROM artifacts WHERE id=?", [id], |r| r.get(0)).optional().map_err(HostError::storage)?;
        serde_json::from_str(&raw.ok_or_else(|| HostError::invalid("artifact not found"))?).map_err(Into::into)
    }

    pub fn grant(&self, artifacts: &[ArtifactRef], target: &ConversationRef) -> Result<(), HostError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(HostError::storage)?;
        for a in artifacts {
            tx.execute("INSERT OR IGNORE INTO artifact_grants VALUES (?,?)", params![a.id, target.grant_key()])
                .map_err(HostError::storage)?;
        }
        tx.commit().map_err(HostError::storage)
    }

    pub fn authorized(&self, id: &str, principals: &[String]) -> Result<bool, HostError> {
        let conn = self.lock()?;
        for principal in principals {
            let found: bool = conn
                .query_row("SELECT EXISTS(SELECT 1 FROM artifact_grants WHERE artifact=? AND principal=?)", params![id, principal], |r| {
                    r.get(0)
                })
                .map_err(HostError::storage)?;
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Validating the id through metadata avoids turning an HTTP/tool id into an arbitrary path.
    pub fn verified_path(&self, id: &str) -> Result<PathBuf, HostError> {
        let a = self.artifact(id)?;
        let path = self.root.join("artifacts").join(&a.id).join("content");
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.is_file() || meta.len() != a.size {
            return Err(HostError::storage("artifact is missing or damaged"));
        }
        let mut file = File::open(&path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        if format!("{:x}", hasher.finalize()) != a.sha256 {
            return Err(HostError::storage("artifact checksum mismatch"));
        }
        Ok(path)
    }

    pub fn fetch(&self, id: &str, workspace: &Path) -> Result<PathBuf, HostError> {
        let a = self.artifact(id)?;
        let source = self.verified_path(id)?;
        let root = workspace.canonicalize()?;
        let parent = root.join("incoming").join(&a.id);
        // Reject pre-existing symlinks at every generated path component.
        for p in [root.join("incoming"), parent.clone()] {
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
                Ok(_) => return Err(HostError::invalid("artifact destination is not a regular directory")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&p)?,
                Err(e) => return Err(e.into()),
            }
        }
        let dest = parent.join(&a.name);
        if let Ok(m) = std::fs::symlink_metadata(&dest) {
            if m.is_file() && !m.file_type().is_symlink() {
                let mut h = Sha256::new();
                std::io::copy(&mut File::open(&dest)?, &mut h)?;
                if format!("{:x}", h.finalize()) == a.sha256 {
                    return Ok(dest);
                }
            }
            return Err(HostError::invalid("the fetched file was changed; refusing to overwrite it"));
        }
        let mut output = OpenOptions::new().write(true).create_new(true).open(&dest)?;
        let result = (|| {
            std::io::copy(&mut File::open(source)?, &mut output)?;
            output.sync_all()?;
            Ok::<_, HostError>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&dest);
        }
        result?;
        Ok(dest)
    }

    pub fn task(&self, id: &str) -> Result<TaskRecord, HostError> {
        let raw: Option<String> =
            self.lock()?.query_row("SELECT data FROM tasks WHERE id=?", [id], |r| r.get(0)).optional().map_err(HostError::storage)?;
        serde_json::from_str(&raw.ok_or_else(|| HostError::invalid("task not found"))?).map_err(Into::into)
    }
    pub fn task_by_key(&self, key: &str) -> Result<Option<TaskRecord>, HostError> {
        let raw: Option<String> = self
            .lock()?
            .query_row("SELECT data FROM tasks WHERE request_key=?", [key], |r| r.get(0))
            .optional()
            .map_err(HostError::storage)?;
        raw.map(|s| serde_json::from_str(&s).map_err(Into::into)).transpose()
    }
    pub fn tasks(&self) -> Result<Vec<TaskRecord>, HostError> {
        let conn = self.lock()?;
        let mut q = conn.prepare("SELECT data FROM tasks ORDER BY rowid").map_err(HostError::storage)?;
        let rows = q.query_map([], |r| r.get::<_, String>(0)).map_err(HostError::storage)?;
        rows.map(|r| Ok(serde_json::from_str(&r.map_err(HostError::storage)?)?)).collect()
    }

    /// Optimistic revision check + state + grants + outbox in one SQLite transaction.
    pub fn commit_task(
        &self,
        previous: Option<u64>,
        task: &TaskRecord,
        deliveries: &[TaskDelivery],
        key: Option<&str>,
    ) -> Result<(), HostError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction().map_err(HostError::storage)?;
        let data = serde_json::to_string(task)?;
        if let Some(rev) = previous {
            let n = tx
                .execute(
                    "UPDATE tasks SET revision=?,data=? WHERE id=? AND revision=?",
                    params![task.revision as i64, data, task.id, rev as i64],
                )
                .map_err(HostError::storage)?;
            if n != 1 {
                return Err(HostError::invalid("task changed concurrently; query its current state and retry"));
            }
        } else {
            tx.execute(
                "INSERT INTO tasks(id,revision,request_key,data) VALUES (?,?,?,?)",
                params![task.id, task.revision as i64, key, data],
            )
            .map_err(HostError::storage)?;
        }
        for a in &task.inputs {
            tx.execute(
                "INSERT OR IGNORE INTO artifact_grants VALUES (?,?)",
                params![a.id, ConversationRef::Agent(task.executor.clone()).grant_key()],
            )
            .map_err(HostError::storage)?;
        }
        for a in &task.artifacts {
            tx.execute("INSERT OR IGNORE INTO artifact_grants VALUES (?,?)", params![a.id, task.origin.grant_key()])
                .map_err(HostError::storage)?;
        }
        for event in deliveries {
            tx.execute("INSERT OR IGNORE INTO task_deliveries VALUES (?,?)", params![event.id, serde_json::to_string(event)?])
                .map_err(HostError::storage)?;
        }
        tx.commit().map_err(HostError::storage)
    }

    pub fn deliveries(&self) -> Result<Vec<TaskDelivery>, HostError> {
        let conn = self.lock()?;
        let mut q = conn.prepare("SELECT data FROM task_deliveries ORDER BY rowid").map_err(HostError::storage)?;
        let rows = q.query_map([], |r| r.get::<_, String>(0)).map_err(HostError::storage)?;
        let all: Vec<TaskDelivery> =
            rows.map(|r| Ok(serde_json::from_str(&r.map_err(HostError::storage)?)?)).collect::<Result<_, HostError>>()?;
        Ok(all.into_iter().filter(|d| !d.done).collect())
    }
    pub fn save_delivery(&self, event: &TaskDelivery) -> Result<(), HostError> {
        self.lock()?
            .execute("UPDATE task_deliveries SET data=? WHERE id=?", params![serde_json::to_string(event)?, event.id])
            .map_err(HostError::storage)?;
        Ok(())
    }
    pub fn delivery(&self, id: &str) -> Result<TaskDelivery, HostError> {
        let raw: String =
            self.lock()?.query_row("SELECT data FROM task_deliveries WHERE id=?", [id], |r| r.get(0)).map_err(HostError::storage)?;
        Ok(serde_json::from_str(&raw)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_binary_snapshots_integrity_limits_and_no_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let store = CoordinationStore::open(root.path()).unwrap();
        let source = root.path().join("input.bin");
        let original: Vec<u8> = (0..32768).map(|n| (n % 256) as u8).collect();
        std::fs::write(&source, &original).unwrap();
        let owner = AgentId::from("owner");
        assert!(store.snapshot(&owner, &source, 1024).is_err());
        assert!(store.snapshot(&owner, root.path(), 999999).is_err());
        let a = store.snapshot(&owner, &source, 999999).unwrap();
        assert!(store.authorized(&a.id, &["agent:owner".into()]).unwrap());
        assert!(!store.authorized(&a.id, &["agent:other".into()]).unwrap());
        std::fs::write(source, b"changed").unwrap();
        assert_eq!(std::fs::read(store.verified_path(&a.id).unwrap()).unwrap(), original);
        let workspace = root.path().join("receiver");
        std::fs::create_dir(&workspace).unwrap();
        let dest = store.fetch(&a.id, &workspace).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), original);
        assert_eq!(store.fetch(&a.id, &workspace).unwrap(), dest);
        std::fs::write(&dest, b"user edit").unwrap();
        assert!(store.fetch(&a.id, &workspace).is_err());
        assert_eq!(std::fs::read(dest).unwrap(), b"user edit");
        let blob = store.verified_path(&a.id).unwrap();
        std::fs::write(blob, b"corruption").unwrap();
        assert!(store.verified_path(&a.id).is_err());
        assert!(store.verified_path("../../etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fetch_rejects_destination_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let store = CoordinationStore::open(root.path()).unwrap();
        let source = root.path().join("input.txt");
        std::fs::write(&source, b"file").unwrap();
        let a = store.snapshot(&AgentId::from("owner"), &source, 100).unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("incoming")).unwrap();
        assert!(store.fetch(&a.id, &workspace).is_err());
        assert!(outside.read_dir().unwrap().next().is_none());
    }
}
