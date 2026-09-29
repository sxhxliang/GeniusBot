//! Directory isolation for local tools.

use gns_core::ToolError;
use std::path::{Component, Path, PathBuf};

/// Resolve `requested` (absolute or relative to `workspace`) and verify it
/// lies inside one of `roots` after symlink resolution of the existing prefix.
pub fn sandbox_path(requested: &str, workspace: &Path, roots: &[&Path]) -> Result<PathBuf, ToolError> {
    let raw = Path::new(requested.trim());
    if raw.as_os_str().is_empty() {
        return Err(ToolError::input("path is required"));
    }
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { workspace.join(raw) };
    let normalized = normalize(&joined);
    let resolved = resolve_existing_prefix(&normalized)?;
    let allowed = roots.iter().any(|root| {
        let root_resolved = root.canonicalize().unwrap_or_else(|_| normalize(root));
        resolved.starts_with(&root_resolved)
    });
    if !allowed {
        return Err(ToolError::denied(format!(
            "{} is outside your directories ({}). Work inside your workspace or ask the user for the file.",
            normalized.display(),
            roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(normalized)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the longest existing prefix so symlink escapes are caught,
/// then re-append the missing tail. A dangling symlink on the way (its
/// target does not exist yet) is followed by hand, so writing "through" it
/// is checked against where the file would actually land.
fn resolve_existing_prefix(path: &Path) -> Result<PathBuf, ToolError> {
    resolve_existing_prefix_inner(path, 0)
}

const MAX_SYMLINK_HOPS: usize = 32;

fn resolve_existing_prefix_inner(path: &Path, hops: usize) -> Result<PathBuf, ToolError> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        if is_symlink(&existing) {
            // Dangling link: resolve its target ourselves and keep going.
            if hops >= MAX_SYMLINK_HOPS {
                return Err(ToolError::denied(format!("{} is a symlink chain too deep to resolve", path.display())));
            }
            let target =
                std::fs::read_link(&existing).map_err(|e| ToolError::failed(format!("cannot read symlink {}: {e}", existing.display())))?;
            let target = if target.is_absolute() { target } else { existing.parent().map(|p| p.join(&target)).unwrap_or(target) };
            let mut redirected = normalize(&target);
            for part in tail.iter().rev() {
                redirected.push(part);
            }
            return resolve_existing_prefix_inner(&redirected, hops + 1);
        }
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_owned());
                existing.pop();
            }
            None => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_inside_and_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ok = sandbox_path("sub/file.txt", &ws, &[&ws]).unwrap();
        assert!(ok.starts_with(&ws));
        assert!(sandbox_path("../outside.txt", &ws, &[&ws]).is_err());
        assert!(sandbox_path("/etc/passwd", &ws, &[&ws]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
        assert!(sandbox_path("link/secret", &ws, &[&ws]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_pointing_outside_but_allows_one_inside() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        // Target does not exist yet: a plain exists() check would miss it.
        std::os::unix::fs::symlink(dir.path().join("outside").join("evil.txt"), ws.join("dangling.txt")).unwrap();
        assert!(sandbox_path("dangling.txt", &ws, &[&ws]).is_err(), "write through a dangling link must not escape");
        // A dangling link to a directory outside, with a tail appended.
        std::os::unix::fs::symlink(dir.path().join("nowhere"), ws.join("dir-link")).unwrap();
        assert!(sandbox_path("dir-link/new.txt", &ws, &[&ws]).is_err());
        // Relative dangling link that stays inside the workspace is fine.
        std::os::unix::fs::symlink("sub/later.txt", ws.join("inside.txt")).unwrap();
        assert!(sandbox_path("inside.txt", &ws, &[&ws]).is_ok());
        // Self-referencing loop is reported, not spun on.
        std::os::unix::fs::symlink("loop", ws.join("loop")).unwrap();
        assert!(sandbox_path("loop", &ws, &[&ws]).is_err());
    }
}
