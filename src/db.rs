//! SQLite registry. Single shared `Arc<Mutex<Connection>>` for the entire
//! daemon; SQLite WAL + serialized access through the mutex is plenty for
//! this workload.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub type SharedDb = Arc<Mutex<Connection>>;

pub const TARGET_USER_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "dir",
        }
    }
    pub fn parse(s: &str) -> Result<Kind> {
        match s {
            "file" => Ok(Kind::File),
            "dir" => Ok(Kind::Dir),
            other => anyhow::bail!("unknown kind: {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub id: i64,
    pub url_path: String,
    pub source_path: String,
    pub kind: Kind,
    pub name_slug: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub ttl_seconds: Option<i64>,
    pub last_hit_at: Option<i64>,
    pub missing_since: Option<i64>,
}

impl Registration {
    pub fn source_pathbuf(&self) -> PathBuf {
        PathBuf::from(&self.source_path)
    }
}

#[derive(Debug, Clone)]
pub struct NewRegistration {
    pub url_path: String,
    pub source_path: String,
    pub kind: Kind,
    pub name_slug: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneStats {
    pub expired: u64,
    pub missing_source: u64,
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("opening db at {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

pub fn migrate(conn: &mut Connection) -> Result<()> {
    loop {
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v >= TARGET_USER_VERSION {
            break;
        }
        let next = v + 1;
        let tx = conn.transaction()?;
        apply_migration(&tx, next)?;
        tx.pragma_update(None, "user_version", next)?;
        tx.commit()?;
    }
    Ok(())
}

fn apply_migration(tx: &rusqlite::Transaction<'_>, v: i64) -> Result<()> {
    match v {
        1 => {
            tx.execute_batch(
                r#"
                CREATE TABLE registrations (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    url_path      TEXT NOT NULL UNIQUE,
                    source_path   TEXT NOT NULL,
                    kind          TEXT NOT NULL CHECK (kind IN ('file','dir')),
                    name_slug     TEXT,
                    created_at    INTEGER NOT NULL,
                    expires_at    INTEGER,
                    ttl_seconds   INTEGER,
                    last_hit_at   INTEGER,
                    missing_since INTEGER
                );
                CREATE INDEX idx_registrations_expires_at  ON registrations(expires_at);
                CREATE INDEX idx_registrations_source_path ON registrations(source_path);
                "#,
            )?;
        }
        other => anyhow::bail!("no migration defined for version {other}"),
    }
    Ok(())
}

fn row_to_registration(row: &rusqlite::Row<'_>) -> rusqlite::Result<Registration> {
    let kind_s: String = row.get("kind")?;
    let kind = match kind_s.as_str() {
        "file" => Kind::File,
        _ => Kind::Dir,
    };
    Ok(Registration {
        id: row.get("id")?,
        url_path: row.get("url_path")?,
        source_path: row.get("source_path")?,
        kind,
        name_slug: row.get("name_slug")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        ttl_seconds: row.get("ttl_seconds")?,
        last_hit_at: row.get("last_hit_at")?,
        missing_since: row.get("missing_since")?,
    })
}

pub fn insert(conn: &Connection, r: &NewRegistration) -> Result<Registration> {
    conn.execute(
        r#"INSERT INTO registrations
            (url_path, source_path, kind, name_slug, created_at, expires_at, ttl_seconds)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
        params![
            r.url_path,
            r.source_path,
            r.kind.as_str(),
            r.name_slug,
            r.created_at,
            r.expires_at,
            r.ttl_seconds,
        ],
    )?;
    let id = conn.last_insert_rowid();
    find_by_id(conn, id)?.context("inserted row vanished")
}

pub fn find_by_id(conn: &Connection, id: i64) -> Result<Option<Registration>> {
    Ok(conn
        .query_row(
            "SELECT * FROM registrations WHERE id = ?1",
            params![id],
            row_to_registration,
        )
        .optional()?)
}

pub fn find_by_source(conn: &Connection, source: &Path) -> Result<Option<Registration>> {
    let s = source.to_string_lossy().to_string();
    let now = now_unix();
    Ok(conn
        .query_row(
            "SELECT * FROM registrations
             WHERE source_path = ?1
               AND (expires_at IS NULL OR expires_at >= ?2)
             LIMIT 1",
            params![s, now],
            row_to_registration,
        )
        .optional()?)
}

pub fn find_by_url(conn: &Connection, url_path: &str) -> Result<Option<Registration>> {
    // Raw URL lookup intentionally includes expired rows. The URL column is
    // UNIQUE, so allocators must still see not-yet-reaped expired rows and
    // suffix around them instead of trying an insert that would fail.
    Ok(conn
        .query_row(
            "SELECT * FROM registrations WHERE url_path = ?1",
            params![url_path],
            row_to_registration,
        )
        .optional()?)
}

pub fn list(conn: &Connection) -> Result<Vec<Registration>> {
    let now = now_unix();
    let mut stmt = conn.prepare(
        "SELECT * FROM registrations
         WHERE expires_at IS NULL OR expires_at >= ?1
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![now], row_to_registration)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute("DELETE FROM registrations WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

pub fn touch(conn: &Connection, id: i64, now: i64, ttl_seconds: Option<i64>) -> Result<()> {
    if ttl_seconds.is_none() {
        // `--ttl never` rows are intentionally not touched; there is no
        // expiry to slide, and the plan requires avoiding pointless UPDATEs.
        return Ok(());
    }
    // Refuse to resurrect rows that have already expired. Without this
    // guard a TouchDebouncer flush queued while the row was still live
    // can fire after the deadline (e.g. on shutdown, after the reaper
    // would have swept the row, or after `prune_on_load` would have on
    // the next startup) and slide expires_at into the future — turning
    // a dead row into a live one. The guard is enforced in SQL so every
    // caller is safe by construction.
    conn.execute(
        r#"UPDATE registrations
           SET last_hit_at = ?1,
               expires_at = CASE WHEN ttl_seconds IS NULL THEN expires_at
                                ELSE ?1 + ttl_seconds END
           WHERE id = ?2
             AND ttl_seconds IS NOT NULL
             AND expires_at IS NOT NULL
             AND expires_at >= ?1"#,
        params![now, id],
    )?;
    Ok(())
}

pub fn mark_missing(conn: &Connection, id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE registrations SET missing_since = ?1 WHERE id = ?2 AND missing_since IS NULL",
        params![now, id],
    )?;
    Ok(())
}

pub fn clear_missing(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE registrations SET missing_since = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Exact-file then longest-prefix-dir lookup for the serving plane.
pub struct ServeMatch {
    pub registration: Registration,
    /// For dir matches, the request-path suffix after the mount prefix
    /// (always starts with `/` or is empty). `None` for file matches.
    pub suffix: Option<String>,
}

pub fn find_for_request(conn: &Connection, request_path: &str) -> Result<Option<ServeMatch>> {
    let now = now_unix();
    // exact file
    if let Some(r) = conn
        .query_row(
            "SELECT * FROM registrations
             WHERE kind = 'file'
               AND url_path = ?1
               AND (expires_at IS NULL OR expires_at >= ?2)",
            params![request_path, now],
            row_to_registration,
        )
        .optional()?
    {
        return Ok(Some(ServeMatch {
            registration: r,
            suffix: None,
        }));
    }
    // longest-prefix dir; dir url_paths always end with '/'.
    // NOTE: we deliberately avoid `LIKE url_path || '%'` here. `url_path`
    // is user-derived (e.g. `/my_project/`), and LIKE would treat any `_`
    // or `%` characters in the stored value as wildcards, silently aliasing
    // unrelated requests onto the wrong mount. `substr(...)=url_path` is a
    // literal byte-prefix test with no metacharacters.
    let mut stmt = conn.prepare(
        "SELECT * FROM registrations
         WHERE kind = 'dir'
           AND substr(?1, 1, length(url_path)) = url_path
           AND (expires_at IS NULL OR expires_at >= ?2)
         ORDER BY length(url_path) DESC
         LIMIT 1",
    )?;
    let reg = stmt
        .query_row(params![request_path, now], row_to_registration)
        .optional()?;
    if let Some(r) = reg {
        // Belt-and-braces: if for any reason the request doesn't strictly
        // start with the stored mount prefix, refuse rather than serving
        // the mount root with an empty suffix.
        match request_path.strip_prefix(&r.url_path) {
            Some(suffix) => {
                return Ok(Some(ServeMatch {
                    registration: r,
                    suffix: Some(suffix.to_string()),
                }));
            }
            None => return Ok(None),
        }
    }
    Ok(None)
}

pub fn count(conn: &Connection) -> Result<i64> {
    let now = now_unix();
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM registrations WHERE expires_at IS NULL OR expires_at >= ?1",
        params![now],
        |r| r.get(0),
    )?)
}

pub fn prune_on_load(conn: &Connection) -> Result<PruneStats> {
    let now = now_unix();
    let expired = conn.execute(
        "DELETE FROM registrations WHERE expires_at IS NOT NULL AND expires_at < ?1",
        params![now],
    )? as u64;

    // Missing source: walk remaining rows, drop ones with explicit NotFound.
    let to_check: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, url_path, source_path FROM registrations")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut missing_source = 0u64;
    for (id, url, source) in to_check {
        match std::fs::metadata(&source) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                conn.execute("DELETE FROM registrations WHERE id = ?1", params![id])?;
                tracing::info!(target: "servant", "pruned registration id={id} url={url} source={source} reason=source-missing");
                missing_source += 1;
            }
            Err(_) => {
                // permission denied / other transient — leave the row.
            }
        }
    }
    Ok(PruneStats {
        expired,
        missing_source,
    })
}

pub fn reap_expired(conn: &Connection) -> Result<u64> {
    let now = now_unix();
    let n = conn.execute(
        "DELETE FROM registrations WHERE expires_at IS NOT NULL AND expires_at < ?1",
        params![now],
    )?;
    Ok(n as u64)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn migration_is_idempotent() {
        let mut c = fresh();
        migrate(&mut c).unwrap();
        let v: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, TARGET_USER_VERSION);
    }

    #[test]
    fn insert_and_find() {
        let c = fresh();
        let n = NewRegistration {
            url_path: "/a.html".into(),
            source_path: "/tmp/a.html".into(),
            kind: Kind::File,
            name_slug: None,
            created_at: 100,
            expires_at: Some(now_unix() + 200),
            ttl_seconds: Some(100),
        };
        let r = insert(&c, &n).unwrap();
        assert_eq!(r.url_path, "/a.html");
        assert!(find_by_url(&c, "/a.html").unwrap().is_some());
        assert!(find_by_source(&c, Path::new("/tmp/a.html"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn unique_url_path() {
        let c = fresh();
        let n = NewRegistration {
            url_path: "/x".into(),
            source_path: "/tmp/x".into(),
            kind: Kind::File,
            name_slug: None,
            created_at: 0,
            expires_at: None,
            ttl_seconds: None,
        };
        insert(&c, &n).unwrap();
        let dup = NewRegistration {
            source_path: "/tmp/y".into(),
            ..n.clone()
        };
        assert!(insert(&c, &dup).is_err());
    }

    #[test]
    fn touch_bumps_expiry() {
        let c = fresh();
        // Row is still live (now=150 < expires_at=200): touch slides forward.
        let r = insert(
            &c,
            &NewRegistration {
                url_path: "/a".into(),
                source_path: "/s/a".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: 100,
                expires_at: Some(200),
                ttl_seconds: Some(100),
            },
        )
        .unwrap();
        touch(&c, r.id, 150, Some(100)).unwrap();
        let r2 = find_by_id(&c, r.id).unwrap().unwrap();
        assert_eq!(r2.expires_at, Some(250));
        assert_eq!(r2.last_hit_at, Some(150));
    }

    #[test]
    fn touch_does_not_resurrect_expired_row() {
        let c = fresh();
        let r = insert(
            &c,
            &NewRegistration {
                url_path: "/a".into(),
                source_path: "/s/a".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: 100,
                expires_at: Some(200),
                ttl_seconds: Some(100),
            },
        )
        .unwrap();
        // Pretend the row expired at 200, but we are now at 300.
        // touch must NOT slide expires_at into the future.
        touch(&c, r.id, 300, Some(100)).unwrap();
        let r2 = find_by_id(&c, r.id).unwrap().unwrap();
        assert_eq!(
            r2.expires_at,
            Some(200),
            "expired row must not be resurrected"
        );
        assert_eq!(
            r2.last_hit_at, None,
            "last_hit_at must not advance for an expired row"
        );
    }

    #[test]
    fn touch_never_skips_update() {
        let c = fresh();
        let r = insert(
            &c,
            &NewRegistration {
                url_path: "/a".into(),
                source_path: "/s/a".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        touch(&c, r.id, 500, None).unwrap();
        let r2 = find_by_id(&c, r.id).unwrap().unwrap();
        assert_eq!(r2.expires_at, None);
        assert_eq!(r2.last_hit_at, None);
    }

    #[test]
    fn longest_prefix_dir_match() {
        let c = fresh();
        insert(
            &c,
            &NewRegistration {
                url_path: "/site/".into(),
                source_path: "/s/site".into(),
                kind: Kind::Dir,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        insert(
            &c,
            &NewRegistration {
                url_path: "/site/sub/".into(),
                source_path: "/s/sub".into(),
                kind: Kind::Dir,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        let m = find_for_request(&c, "/site/sub/x.png").unwrap().unwrap();
        assert_eq!(m.registration.url_path, "/site/sub/");
        assert_eq!(m.suffix.as_deref(), Some("x.png"));
    }

    #[test]
    fn underscore_in_url_path_is_not_a_like_wildcard() {
        // Regression: a dir mount whose url_path contains `_` must not
        // match arbitrary single characters via LIKE. A request for an
        // unrelated `/myXproject/...` path must NOT alias onto
        // `/my_project/`.
        let c = fresh();
        insert(
            &c,
            &NewRegistration {
                url_path: "/my_project/".into(),
                source_path: "/s/mp".into(),
                kind: Kind::Dir,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();

        // Exact prefix match still works.
        let ok = find_for_request(&c, "/my_project/index.html")
            .unwrap()
            .unwrap();
        assert_eq!(ok.registration.url_path, "/my_project/");
        assert_eq!(ok.suffix.as_deref(), Some("index.html"));

        // `_` must not act as a single-char wildcard.
        assert!(find_for_request(&c, "/myXproject/index.html")
            .unwrap()
            .is_none());
        assert!(find_for_request(&c, "/my-project/index.html")
            .unwrap()
            .is_none());
    }

    #[test]
    fn overlapping_underscore_dirs_match_only_literal_prefix() {
        // Regression: when an underscore-containing mount and a similarly
        // shaped literal mount both exist, a request must resolve to the
        // literal prefix it actually starts with, never whichever LIKE match
        // SQLite happens to return for an equal-length wildcard pattern.
        let c = fresh();
        insert(
            &c,
            &NewRegistration {
                url_path: "/my_project/".into(),
                source_path: "/s/underscore".into(),
                kind: Kind::Dir,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        insert(
            &c,
            &NewRegistration {
                url_path: "/myXproject/".into(),
                source_path: "/s/literal".into(),
                kind: Kind::Dir,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();

        let m = find_for_request(&c, "/myXproject/index.html")
            .unwrap()
            .unwrap();
        assert_eq!(m.registration.url_path, "/myXproject/");
        assert_eq!(m.suffix.as_deref(), Some("index.html"));
    }

    #[test]
    fn percent_in_url_path_is_not_a_like_wildcard() {
        // Same defense for `%` (the multi-char LIKE wildcard). url_alloc
        // sanitizes names today, but the storage-layer lookup must not
        // depend on that invariant.
        let c = fresh();
        insert(
            &c,
            &NewRegistration {
                url_path: "/a%b/".into(),
                source_path: "/s/ab".into(),
                kind: Kind::Dir,
                name_slug: None,
                created_at: 0,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
        let ok = find_for_request(&c, "/a%b/x").unwrap().unwrap();
        assert_eq!(ok.registration.url_path, "/a%b/");
        assert_eq!(ok.suffix.as_deref(), Some("x"));
        // `%` must not match "anything".
        assert!(find_for_request(&c, "/aZZZb/x").unwrap().is_none());
        assert!(find_for_request(&c, "/ab/x").unwrap().is_none());
    }

    #[test]
    fn expired_rows_are_hidden_from_runtime_lookups() {
        let c = fresh();
        let now = now_unix();
        insert(
            &c,
            &NewRegistration {
                url_path: "/old".into(),
                source_path: "/tmp/old".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now - 10,
                expires_at: Some(now - 1),
                ttl_seconds: Some(1),
            },
        )
        .unwrap();
        insert(
            &c,
            &NewRegistration {
                url_path: "/live".into(),
                source_path: "/tmp/live".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now,
                expires_at: Some(now + 60),
                ttl_seconds: Some(60),
            },
        )
        .unwrap();

        assert!(find_by_source(&c, Path::new("/tmp/old")).unwrap().is_none());
        assert!(find_for_request(&c, "/old").unwrap().is_none());
        assert_eq!(count(&c).unwrap(), 1);
        let rows = list(&c).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url_path, "/live");
    }
}
