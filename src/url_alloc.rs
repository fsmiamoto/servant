//! URL path allocation for `POST /register`: handles idempotency,
//! collision auto-suffixing, sanitization of `--name`, and the reserved
//! `_servant` namespace.

use std::path::Path;

use anyhow::{anyhow, Result};
use rusqlite::Connection;

use crate::db::{self, Kind, Registration};

const MAX_SUFFIX_ATTEMPTS: u32 = 1000;
const RESERVED_NAMES: &[&str] = &["_servant"];

pub enum UrlAllocation {
    Existing(Registration),
    New {
        url_path: String,
        name_slug: Option<String>,
    },
}

pub fn allocate_url_path(
    conn: &Connection,
    source: &Path,
    kind: Kind,
    name: Option<&str>,
) -> Result<UrlAllocation> {
    if let Some(existing) = db::find_by_source(conn, source)? {
        return Ok(UrlAllocation::Existing(existing));
    }

    let (base, name_slug) = base_url_path(source, kind.clone(), name)?;

    if db::find_by_url(conn, &base)?.is_none() {
        return Ok(UrlAllocation::New {
            url_path: base,
            name_slug,
        });
    }

    for n in 2..=MAX_SUFFIX_ATTEMPTS {
        let candidate = match kind {
            Kind::File => suffix_file(&base, n),
            Kind::Dir => suffix_dir(&base, n),
        };
        if db::find_by_url(conn, &candidate)?.is_none() {
            return Ok(UrlAllocation::New {
                url_path: candidate,
                name_slug,
            });
        }
    }
    Err(anyhow!(
        "could not allocate a unique URL path after {MAX_SUFFIX_ATTEMPTS} attempts"
    ))
}

fn base_url_path(
    source: &Path,
    kind: Kind,
    name: Option<&str>,
) -> Result<(String, Option<String>)> {
    if let Some(raw) = name {
        let slug = sanitize_name(raw)?;
        let base = match kind {
            Kind::File => format!("/{slug}"),
            Kind::Dir => format!("/{slug}/"),
        };
        return Ok((base, Some(slug)));
    }
    let file_name = source
        .file_name()
        .ok_or_else(|| anyhow!("source path has no final component: {}", source.display()))?
        .to_string_lossy()
        .to_string();
    if RESERVED_NAMES.contains(&file_name.as_str()) {
        return Err(anyhow!(
            "`{file_name}` is reserved; use --name to choose another slug"
        ));
    }
    let base = match kind {
        Kind::File => format!("/{file_name}"),
        Kind::Dir => format!("/{file_name}/"),
    };
    Ok((base, None))
}

fn sanitize_name(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(anyhow!("--name must not be empty"));
    }
    if RESERVED_NAMES.contains(&raw) {
        return Err(anyhow!("`{raw}` is reserved"));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(anyhow!("--name must match [A-Za-z0-9._-]+ (got `{raw}`)"));
    }
    Ok(raw.to_string())
}

/// Insert a numeric suffix before the first dot for files.
/// `"/foo.tar.gz"` + 2 → `"/foo-2.tar.gz"`.
fn suffix_file(base: &str, n: u32) -> String {
    debug_assert!(base.starts_with('/'));
    let body = &base[1..];
    let (stem, ext) = split_filename(body);
    match ext {
        Some(e) => format!("/{stem}-{n}{e}"),
        None => format!("/{body}-{n}"),
    }
}

/// `"/site/"` + 2 → `"/site-2/"`.
fn suffix_dir(base: &str, n: u32) -> String {
    debug_assert!(base.starts_with('/') && base.ends_with('/'));
    let body = &base[1..base.len() - 1];
    format!("/{body}-{n}/")
}

/// First-dot split: `"foo.tar.gz"` → `("foo", Some(".tar.gz"))`.
fn split_filename(name: &str) -> (&str, Option<&str>) {
    match name.find('.') {
        // Hidden file `.env` → no extension split.
        Some(0) => (name, None),
        Some(i) => (&name[..i], Some(&name[i..])),
        None => (name, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrate, NewRegistration};
    use std::path::PathBuf;

    fn fresh() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn split_first_dot() {
        assert_eq!(split_filename("foo.tar.gz"), ("foo", Some(".tar.gz")));
        assert_eq!(split_filename("foo.html"), ("foo", Some(".html")));
        assert_eq!(split_filename("foo"), ("foo", None));
        assert_eq!(split_filename(".env"), (".env", None));
    }

    #[test]
    fn suffix_file_double_ext() {
        assert_eq!(suffix_file("/foo.tar.gz", 2), "/foo-2.tar.gz");
        assert_eq!(suffix_file("/a.html", 3), "/a-3.html");
        assert_eq!(suffix_file("/x", 2), "/x-2");
    }

    #[test]
    fn suffix_dir_basic() {
        assert_eq!(suffix_dir("/site/", 2), "/site-2/");
        assert_eq!(suffix_dir("/my.dir/", 9), "/my.dir-9/");
    }

    #[test]
    fn idempotent_returns_existing() {
        let c = fresh();
        let src = PathBuf::from("/tmp/x.html");
        let _ = db::insert(
            &c,
            &NewRegistration {
                url_path: "/x.html".into(),
                source_path: src.to_string_lossy().into(),
                kind: Kind::File,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        match allocate_url_path(&c, &src, Kind::File, None).unwrap() {
            UrlAllocation::Existing(r) => assert_eq!(r.url_path, "/x.html"),
            UrlAllocation::New { .. } => panic!("expected Existing"),
        }
    }

    #[test]
    fn auto_suffix_on_collision() {
        let c = fresh();
        let _ = db::insert(
            &c,
            &NewRegistration {
                url_path: "/a.html".into(),
                source_path: "/one/a.html".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        let alloc = allocate_url_path(&c, Path::new("/two/a.html"), Kind::File, None).unwrap();
        match alloc {
            UrlAllocation::New { url_path, .. } => assert_eq!(url_path, "/a-2.html"),
            _ => panic!(),
        }
    }

    #[test]
    fn reserved_basename_rejected() {
        let c = fresh();
        let err = allocate_url_path(&c, Path::new("/tmp/_servant"), Kind::File, None);
        assert!(err.is_err());
    }

    #[test]
    fn bad_name_rejected() {
        let c = fresh();
        let err = allocate_url_path(&c, Path::new("/tmp/x"), Kind::File, Some("../etc/passwd"));
        assert!(err.is_err());
        let err = allocate_url_path(&c, Path::new("/tmp/x"), Kind::File, Some("_servant"));
        assert!(err.is_err());
    }

    #[test]
    fn dir_name_gets_trailing_slash() {
        let c = fresh();
        let alloc = allocate_url_path(&c, Path::new("/tmp/site"), Kind::Dir, None).unwrap();
        if let UrlAllocation::New { url_path, .. } = alloc {
            assert_eq!(url_path, "/site/");
        } else {
            panic!();
        }
    }
}
