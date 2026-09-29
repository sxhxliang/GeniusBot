//! Per-agent SQLite store (`store.db`).

use gns_core::{EntryId, HostError, KvStore, TranscriptEntry, TranscriptStore};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS kv (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS transcript_entries (
  seq INTEGER PRIMARY KEY,
  id TEXT NOT NULL UNIQUE,
  entry TEXT NOT NULL
) STRICT;
"#;

/// One agent's database. Cheap synchronous calls guarded by a mutex; SQLite
/// local operations are sub-millisecond so they stay on the async thread.
pub struct AgentDb {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for AgentDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentDb")
    }
}

fn map_err(e: rusqlite::Error) -> HostError {
    HostError::storage(e)
}

impl AgentDb {
    /// Open (or create) the database at `path` in WAL mode.
    pub fn open(path: &Path) -> Result<Self, HostError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).map_err(map_err)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(map_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(map_err)?;
        conn.pragma_update(None, "busy_timeout", 5_000).map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// In-memory database (tests).
    pub fn open_in_memory() -> Result<Self, HostError> {
        let conn = Connection::open_in_memory().map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, HostError> {
        let conn = self.conn.lock().map_err(|_| HostError::storage("db mutex poisoned"))?;
        f(&conn).map_err(map_err)
    }

    /// Delete every transcript entry.
    pub fn clear_transcript(&self) -> Result<(), HostError> {
        self.with_conn(|c| c.execute("DELETE FROM transcript_entries", []).map(|_| ()))
    }

    /// Entries with `seq` strictly greater than `after_seq`, oldest first.
    pub fn entries_after(&self, after_seq: i64, limit: usize) -> Result<Vec<(i64, TranscriptEntry)>, HostError> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT seq, entry FROM transcript_entries WHERE seq > ? ORDER BY seq LIMIT ?")?;
            let rows = stmt.query_map(params![after_seq, limit as i64], |row| {
                let seq: i64 = row.get(0)?;
                let raw: String = row.get(1)?;
                Ok((seq, raw))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (seq, raw) = row?;
                if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(&raw) {
                    out.push((seq, entry));
                }
            }
            Ok(out)
        })
    }

    /// Highest sequence number, or 0 when empty.
    /// A page of entries strictly before `before_seq` (or the newest when
    /// `None`), newest first with their sequence numbers (`getAgentTranscriptPage`).
    pub fn page_before(&self, before_seq: Option<i64>, limit: usize) -> Result<Vec<(i64, TranscriptEntry)>, HostError> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT seq, entry FROM transcript_entries WHERE seq < ? ORDER BY seq DESC LIMIT ?")?;
            let rows =
                stmt.query_map([before_seq.unwrap_or(i64::MAX), limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (seq, raw) = row?;
                if let Ok(entry) = serde_json::from_str(&raw) {
                    out.push((seq, entry));
                }
            }
            Ok(out)
        })
    }
    pub fn last_seq(&self) -> Result<i64, HostError> {
        self.with_conn(|c| c.query_row("SELECT COALESCE(MAX(seq), 0) FROM transcript_entries", [], |r| r.get(0)))
    }
}

impl KvStore for AgentDb {
    fn get(&self, key: &str) -> Result<Option<String>, HostError> {
        self.with_conn(|c| c.query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0)).optional())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), HostError> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
        })
    }
    fn delete(&self, key: &str) -> Result<(), HostError> {
        self.with_conn(|c| c.execute("DELETE FROM kv WHERE key = ?", [key]).map(|_| ()))
    }
    fn compare_and_set(&self, key: &str, expected: &str, value: &str) -> Result<bool, HostError> {
        self.with_conn(|c| c.execute("UPDATE kv SET value = ? WHERE key = ? AND value = ?", params![value, key, expected]).map(|n| n > 0))
    }
}

impl TranscriptStore for AgentDb {
    fn append_entry(&self, entry: &TranscriptEntry) -> Result<i64, HostError> {
        let raw = serde_json::to_string(entry)?;
        let inserted = self.with_conn(|c| {
            let changed =
                c.execute("INSERT OR IGNORE INTO transcript_entries (id, entry) VALUES (?, ?)", params![entry.id().as_str(), raw])?;
            Ok((changed > 0).then(|| c.last_insert_rowid()))
        })?;
        // A duplicate id inserts nothing; `last_insert_rowid()` would then
        // report the previous row's seq as if it were this entry's.
        inserted.ok_or_else(|| HostError::invalid(format!("duplicate transcript entry id {}", entry.id().as_str())))
    }
    fn update_entry(&self, entry: &TranscriptEntry) -> Result<bool, HostError> {
        let raw = serde_json::to_string(entry)?;
        self.with_conn(|c| {
            c.execute("UPDATE transcript_entries SET entry = ? WHERE id = ?", params![raw, entry.id().as_str()]).map(|n| n > 0)
        })
    }
    fn entry(&self, id: &EntryId) -> Result<Option<TranscriptEntry>, HostError> {
        let raw: Option<String> =
            self.with_conn(|c| c.query_row("SELECT entry FROM transcript_entries WHERE id = ?", [id.as_str()], |r| r.get(0)).optional())?;
        match raw {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }
    fn tail(&self, limit: usize) -> Result<Vec<TranscriptEntry>, HostError> {
        let mut rows: Vec<TranscriptEntry> = self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT entry FROM transcript_entries ORDER BY seq DESC LIMIT ?")?;
            let rows = stmt.query_map([limit as i64], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for raw in rows {
                if let Ok(entry) = serde_json::from_str(&raw?) {
                    out.push(entry);
                }
            }
            Ok(out)
        })?;
        rows.reverse();
        Ok(rows)
    }
    fn all(&self) -> Result<Vec<TranscriptEntry>, HostError> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT entry FROM transcript_entries ORDER BY seq")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for raw in rows {
                if let Ok(entry) = serde_json::from_str(&raw?) {
                    out.push(entry);
                }
            }
            Ok(out)
        })
    }
    fn len(&self) -> Result<usize, HostError> {
        self.with_conn(|c| c.query_row("SELECT COUNT(*) FROM transcript_entries", [], |r| r.get::<_, i64>(0)).map(|n| n as usize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gns_core::{KvJson, Role, RunId};

    fn msg(text: &str) -> TranscriptEntry {
        TranscriptEntry::Message {
            artifacts: Vec::new(),
            id: EntryId::new(),
            role: Role::User,
            content: text.into(),
            timestamp_ms: 1,
            hidden: false,
            run_id: Some(RunId::new()),
            from_agent: None,
            to_agent: None,
            images: vec![],
            group_id: None,
            priority: false,
            user_name: None,
            reply_to: None,
        }
    }

    #[test]
    fn kv_roundtrip() {
        let db = AgentDb::open_in_memory().unwrap();
        assert_eq!(db.get("x").unwrap(), None);
        db.set("x", "1").unwrap();
        assert_eq!(db.get("x").unwrap(), Some("1".into()));
        assert!(db.compare_and_set("x", "1", "2").unwrap());
        assert!(!db.compare_and_set("x", "1", "3").unwrap());
        db.set_json("j", &vec![1, 2]).unwrap();
        assert_eq!(db.get_json::<Vec<i32>>("j").unwrap(), Some(vec![1, 2]));
        db.delete("x").unwrap();
        assert_eq!(db.get("x").unwrap(), None);
    }

    #[test]
    fn transcript_order_and_update() {
        let db = AgentDb::open_in_memory().unwrap();
        for i in 0..5 {
            db.append_entry(&msg(&format!("m{i}"))).unwrap();
        }
        let tail = db.tail(2).unwrap();
        assert_eq!(tail.len(), 2);
        assert!(matches!(&tail[1], TranscriptEntry::Message { content, .. } if content == "m4"));
        let mut first = db.all().unwrap().remove(0);
        if let TranscriptEntry::Message { content, .. } = &mut first {
            *content = "edited".into();
        }
        assert!(db.update_entry(&first).unwrap());
        assert!(matches!(db.entry(first.id()).unwrap().unwrap(), TranscriptEntry::Message { content, .. } if content == "edited"));
        assert_eq!(db.len().unwrap(), 5);
        assert_eq!(db.entries_after(3, 10).unwrap().len(), 2);
    }

    #[test]
    fn duplicate_entry_id_is_an_error_not_a_stale_seq() {
        let db = AgentDb::open_in_memory().unwrap();
        let first = msg("a");
        let seq = db.append_entry(&first).unwrap();
        assert!(db.append_entry(&msg("b")).unwrap() > seq);
        let err = db.append_entry(&first).unwrap_err();
        assert!(err.to_string().contains("duplicate transcript entry id"), "{err}");
        assert_eq!(db.len().unwrap(), 2);
    }

    #[test]
    fn opens_on_disk_with_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("store.db");
        let db = AgentDb::open(&path).unwrap();
        db.set("k", "v").unwrap();
        drop(db);
        let db = AgentDb::open(&path).unwrap();
        assert_eq!(db.get("k").unwrap(), Some("v".into()));
    }
}
