//! User-input path normalization: tilde expansion, relative resolution,
//! existence check, and file-vs-dir classification.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::db::Kind;

pub fn canonicalize_user_path(input: &str, cwd: &Path) -> Result<(PathBuf, Kind)> {
    let expanded = expand_tilde(input)?;
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let abs = std::fs::canonicalize(&joined)
        .with_context(|| format!("path not found or unreadable: {}", joined.display()))?;
    let md = std::fs::metadata(&abs).with_context(|| format!("metadata: {}", abs.display()))?;
    let ft = md.file_type();
    if ft.is_file() {
        Ok((abs, Kind::File))
    } else if ft.is_dir() {
        Ok((abs, Kind::Dir))
    } else {
        Err(anyhow!(
            "{} is neither a regular file nor a directory",
            abs.display()
        ))
    }
}

fn expand_tilde(input: &str) -> Result<PathBuf> {
    if input == "~" {
        return crate::config::home_dir();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return Ok(crate::config::home_dir()?.join(rest));
    }
    Ok(PathBuf::from(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn canonicalizes_relative() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::File::create(&p).unwrap().write_all(b"x").unwrap();
        let (got, kind) = canonicalize_user_path("a.txt", dir.path()).unwrap();
        assert_eq!(got, std::fs::canonicalize(&p).unwrap());
        assert_eq!(kind, Kind::File);
    }

    #[test]
    fn classifies_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let (_, kind) = canonicalize_user_path("sub", dir.path()).unwrap();
        assert_eq!(kind, Kind::Dir);
    }

    #[test]
    fn missing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(canonicalize_user_path("nope", dir.path()).is_err());
    }

    #[test]
    fn tilde_expansion() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let f = home.path().join("x");
        std::fs::File::create(&f).unwrap();
        let (got, _) = canonicalize_user_path("~/x", Path::new("/")).unwrap();
        assert_eq!(got, std::fs::canonicalize(&f).unwrap());
    }
}
