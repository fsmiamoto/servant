//! Security guards for folder mounts: `..` traversal, dotfile, and
//! symlink-escape checks.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum GuardOutcome {
    Allow(PathBuf),
    NotFound,
    Forbidden,
}

/// Resolve `suffix` under `mount_root` with the three guards applied.
///
/// `suffix` may start with `/` (or be empty). We never trust raw `..`.
pub fn resolve_under_mount(mount_root: &Path, suffix: &str) -> GuardOutcome {
    let trimmed = suffix.trim_start_matches('/');
    // Strip a trailing slash for resolution; preserve directory intent for callers.
    let path_part = trimmed.trim_end_matches('/');

    for seg in path_part.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == ".." || seg.contains('\0') {
            return GuardOutcome::Forbidden;
        }
        if seg.starts_with('.') {
            return GuardOutcome::NotFound;
        }
    }

    let canonical_root = match std::fs::canonicalize(mount_root) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return GuardOutcome::NotFound,
        Err(_) => return GuardOutcome::Forbidden,
    };

    let joined = if path_part.is_empty() {
        canonical_root.clone()
    } else {
        canonical_root.join(path_part)
    };

    let canonical = match std::fs::canonicalize(&joined) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return GuardOutcome::NotFound,
        Err(_) => return GuardOutcome::Forbidden,
    };

    if !canonical.starts_with(&canonical_root) {
        return GuardOutcome::Forbidden;
    }

    GuardOutcome::Allow(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        match resolve_under_mount(dir.path(), "/../etc") {
            GuardOutcome::Forbidden => {}
            o => panic!("expected Forbidden, got {o:?}"),
        }
    }

    #[test]
    fn rejects_dotfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "x").unwrap();
        match resolve_under_mount(dir.path(), "/.env") {
            GuardOutcome::NotFound => {}
            o => panic!("expected NotFound, got {o:?}"),
        }
    }

    #[test]
    fn rejects_symlink_escape() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        let mount = tempfile::tempdir().unwrap();
        symlink(outside.path().join("secret"), mount.path().join("link")).unwrap();
        match resolve_under_mount(mount.path(), "/link") {
            GuardOutcome::Forbidden => {}
            o => panic!("expected Forbidden, got {o:?}"),
        }
    }

    #[test]
    fn allows_normal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "x").unwrap();
        match resolve_under_mount(dir.path(), "/sub/a.txt") {
            GuardOutcome::Allow(_) => {}
            o => panic!("expected Allow, got {o:?}"),
        }
    }
}
