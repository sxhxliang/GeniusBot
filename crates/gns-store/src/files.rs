//! Small JSON files written atomically (tmp + rename), matching the original
//! `agent-profile.ts` / `settings-file.ts` behaviour.

use gns_core::{AgentProfile, AgentSettings, GroupConfig, HostError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;

/// Write `value` as pretty JSON atomically.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), HostError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut serialized = serde_json::to_string_pretty(value)?;
    serialized.push('\n');
    write_text_atomic(path, &serialized)
}

/// Write text atomically: a uniquely named temp file next to the target,
/// flushed to disk, then renamed over it. Concurrent writers of the same
/// file each get their own temp file; the last rename wins intact.
pub fn write_text_atomic(path: &Path, text: &str) -> Result<(), HostError> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = path.with_extension(format!(
        "{}.tmp-{}-{}-{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("dat"),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        nanos
    ));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Read JSON, returning `None` when the file does not exist.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, HostError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `profile.json`.
#[derive(Debug)]
pub struct ProfileFile;

impl ProfileFile {
    pub fn read(path: &Path) -> Result<AgentProfile, HostError> {
        Ok(read_json::<AgentProfile>(path)?.unwrap_or_default())
    }
    pub fn write(path: &Path, profile: &AgentProfile) -> Result<(), HostError> {
        let mut normalized = profile.clone();
        normalized.title = normalized.title.trim().to_owned();
        normalized.avatar_shape = normalized.avatar_shape.trim().to_owned();
        write_json_atomic(path, &normalized)
    }
}

/// `settings.json`.
#[derive(Debug)]
pub struct SettingsFile;

impl SettingsFile {
    pub fn read(path: &Path) -> Result<AgentSettings, HostError> {
        Ok(read_json::<AgentSettings>(path)?.unwrap_or_default())
    }
    pub fn write(path: &Path, settings: &AgentSettings) -> Result<(), HostError> {
        write_json_atomic(path, settings)
    }
}

/// `group.json`.
#[derive(Debug)]
pub struct GroupFile;

impl GroupFile {
    pub fn read(path: &Path) -> Result<Option<GroupConfig>, HostError> {
        read_json(path)
    }
    pub fn write(path: &Path, group: &GroupConfig) -> Result<(), HostError> {
        write_json_atomic(path, group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("profile.json");
        assert_eq!(ProfileFile::read(&path).unwrap(), AgentProfile::default());
        let profile = AgentProfile {
            name: "A".into(),
            description: "d".into(),
            title: " t ".into(),
            avatar_shape: String::new(),
            avatar_color: String::new(),
        };
        ProfileFile::write(&path, &profile).unwrap();
        let back = ProfileFile::read(&path).unwrap();
        assert_eq!(back.title, "t");
        assert!(!dir.path().join("nested").read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("tmp")));
    }

    #[test]
    fn concurrent_writers_never_corrupt_or_leave_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for j in 0..50 {
                        write_json_atomic(&path, &serde_json::json!({"writer": i, "round": j})).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let back: serde_json::Value = read_json(&path).unwrap().unwrap();
        assert!(back.get("writer").is_some(), "{back}");
        let leftovers: Vec<String> = dir
            .path()
            .read_dir()
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
