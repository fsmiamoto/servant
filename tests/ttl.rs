//! M4 verify: sliding TTL + reaper + never + restart persistence.
//!
//! These tests poke the in-memory DB API directly to keep them fast and
//! deterministic; the user-visible flow is covered indirectly through
//! the control-plane test. The final test drives the real daemon
//! end-to-end through a SIGTERM-equivalent + restart cycle to guard
//! against the resurrection bug fixed in `db::touch`.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// Tests that mutate the process-wide $HOME must be serialized; otherwise
// a second daemon racing into the same HOME hits the single-instance
// lock and `std::process::exit(2)` which tears down the test binary.
fn home_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

use servant::config::Config;
use servant::control::TtlRequest;
use servant::db::{self, Kind, NewRegistration};
use servant::ttl::TouchDebouncer;
use tokio_util::sync::CancellationToken;

fn fresh_shared() -> servant::db::SharedDb {
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    db::migrate(&mut c).unwrap();
    std::sync::Arc::new(std::sync::Mutex::new(c))
}

#[tokio::test]
async fn sliding_ttl_keeps_row_alive_then_expires() {
    let db = fresh_shared();
    // Insert with ttl 2s.
    let now = db::now_unix();
    let r = {
        let conn = db.lock().unwrap();
        db::insert(
            &conn,
            &NewRegistration {
                url_path: "/x".into(),
                source_path: "/tmp/x".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now,
                expires_at: Some(now + 2),
                ttl_seconds: Some(2),
            },
        )
        .unwrap()
    };
    let touch = TouchDebouncer::spawn_with(db.clone(), Duration::from_millis(100));

    // Hit every 100ms for 1s; should keep moving expires_at forward.
    for _ in 0..10 {
        touch.touch(r.id, Some(2));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    touch.flush().await;

    let conn = db.lock().unwrap();
    let cur = db::find_by_id(&conn, r.id).unwrap().unwrap();
    let later = db::now_unix();
    assert!(
        cur.expires_at.unwrap() > now + 2,
        "expires_at should have moved forward (was {}, started at {})",
        cur.expires_at.unwrap(),
        now + 2
    );
    // The reaper would not delete this since expires_at is in the future.
    assert!(cur.expires_at.unwrap() >= later);
}

#[tokio::test]
async fn reaper_deletes_expired() {
    let db = fresh_shared();
    let now = db::now_unix();
    {
        let conn = db.lock().unwrap();
        db::insert(
            &conn,
            &NewRegistration {
                url_path: "/old".into(),
                source_path: "/tmp/old".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now - 100,
                expires_at: Some(now - 1),
                ttl_seconds: Some(50),
            },
        )
        .unwrap();
        db::insert(
            &conn,
            &NewRegistration {
                url_path: "/keep".into(),
                source_path: "/tmp/keep".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now,
                expires_at: Some(now + 3600),
                ttl_seconds: Some(3600),
            },
        )
        .unwrap();
        db::insert(
            &conn,
            &NewRegistration {
                url_path: "/never".into(),
                source_path: "/tmp/never".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap();
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    let h = servant::reaper::spawn_reaper(db.clone(), Duration::from_millis(100), shutdown.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    shutdown.cancel();
    let _ = h.await;

    let conn = db.lock().unwrap();
    let rows = db::list(&conn).unwrap();
    let urls: Vec<&str> = rows.iter().map(|r| r.url_path.as_str()).collect();
    assert!(!urls.contains(&"/old"), "expired row should be reaped");
    assert!(urls.contains(&"/keep"));
    assert!(urls.contains(&"/never"));
}

#[tokio::test]
async fn never_ttl_survives_touch() {
    let db = fresh_shared();
    let now = db::now_unix();
    let r = {
        let conn = db.lock().unwrap();
        db::insert(
            &conn,
            &NewRegistration {
                url_path: "/n".into(),
                source_path: "/tmp/n".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now,
                expires_at: None,
                ttl_seconds: None,
            },
        )
        .unwrap()
    };
    let touch = TouchDebouncer::spawn_with(db.clone(), Duration::from_millis(50));
    for _ in 0..5 {
        touch.touch(r.id, None);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    touch.flush().await;
    let conn = db.lock().unwrap();
    let cur = db::find_by_id(&conn, r.id).unwrap().unwrap();
    assert_eq!(cur.expires_at, None, "never TTL must stay NULL");
    assert_eq!(
        cur.last_hit_at, None,
        "never TTL touches should not issue SQL UPDATEs"
    );
}

#[test]
fn prune_on_load_drops_expired_and_missing_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dbp = tmp.path().join("r.db");
    let mut c = db::open(&dbp).unwrap();
    db::migrate(&mut c).unwrap();
    let now = db::now_unix();

    // Expired row.
    db::insert(
        &c,
        &NewRegistration {
            url_path: "/old".into(),
            source_path: tmp.path().join("present.txt").to_string_lossy().into(),
            kind: Kind::File,
            name_slug: None,
            created_at: 0,
            expires_at: Some(now - 100),
            ttl_seconds: Some(60),
        },
    )
    .unwrap();
    // Live row, but source missing.
    db::insert(
        &c,
        &NewRegistration {
            url_path: "/missing".into(),
            source_path: tmp.path().join("nope.txt").to_string_lossy().into(),
            kind: Kind::File,
            name_slug: None,
            created_at: 0,
            expires_at: Some(now + 3600),
            ttl_seconds: Some(3600),
        },
    )
    .unwrap();
    // Live row, source present.
    let present = tmp.path().join("present.txt");
    std::fs::write(&present, "x").unwrap();
    db::insert(
        &c,
        &NewRegistration {
            url_path: "/live".into(),
            source_path: present.to_string_lossy().into(),
            kind: Kind::File,
            name_slug: None,
            created_at: 0,
            expires_at: Some(now + 3600),
            ttl_seconds: Some(3600),
        },
    )
    .unwrap();

    let stats = db::prune_on_load(&c).unwrap();
    assert_eq!(stats.expired, 1);
    assert_eq!(stats.missing_source, 1);

    let rows = db::list(&c).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].url_path, "/live");
}

// --------------------------------------------------------------------
// Regression: TouchDebouncer flush on shutdown must not resurrect rows
// that have already expired by the time the flush runs.
// --------------------------------------------------------------------

#[tokio::test]
async fn touch_flush_does_not_resurrect_expired_row() {
    let shared = std::sync::Arc::new(std::sync::Mutex::new({
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        db::migrate(&mut c).unwrap();
        c
    }));
    let now = db::now_unix();
    let r = {
        let conn = shared.lock().unwrap();
        db::insert(
            &conn,
            &NewRegistration {
                url_path: "/x".into(),
                source_path: "/tmp/x".into(),
                kind: Kind::File,
                name_slug: None,
                created_at: now,
                expires_at: Some(now + 2),
                ttl_seconds: Some(2),
            },
        )
        .unwrap()
    };

    // Use a coalesce window larger than the TTL so the first touch is
    // captured in the pending map but not re-flushed automatically; the
    // queued entry will sit there past the row's expiry, simulating
    // exactly the shutdown race: a hit queued while live, drained after
    // the deadline has passed.
    let touch = TouchDebouncer::spawn_with(shared.clone(), Duration::from_secs(30));
    touch.touch(r.id, Some(2));

    // Wait well past expiry: first touch slid expires_at to now+2,
    // sleeping 4s puts us comfortably beyond that even allowing for the
    // 1s timestamp granularity.
    tokio::time::sleep(Duration::from_millis(4_000)).await;

    // Capture what the row's expires_at was right before flush; the
    // flush must NOT advance it.
    let before = {
        let conn = shared.lock().unwrap();
        db::find_by_id(&conn, r.id)
            .unwrap()
            .expect("row should still exist pre-flush")
    };
    assert!(
        before.expires_at.unwrap() < db::now_unix(),
        "sanity: row should be past its deadline before flush, got expires_at={:?} now={}",
        before.expires_at,
        db::now_unix()
    );

    // Now flush — historically this resurrected the row.
    touch.flush().await;

    let conn = shared.lock().unwrap();
    let after = db::find_by_id(&conn, r.id)
        .unwrap()
        .expect("row should still exist post-flush");
    assert_eq!(
        after.expires_at, before.expires_at,
        "flush must not move expires_at on an already-expired row"
    );
    assert_eq!(
        after.last_hit_at, before.last_hit_at,
        "flush must not update last_hit_at on an already-expired row"
    );
}

// --------------------------------------------------------------------
// End-to-end: real daemon across an external-shutdown + restart cycle.
// Mirrors `kill <pid>; servant daemon` from the M4 verify checklist.
// --------------------------------------------------------------------

fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn wait_for_socket(p: &std::path::Path, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if p.exists() && tokio::net::UnixStream::connect(p).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

async fn wait_for_socket_gone(p: &std::path::Path, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if !p.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn expired_short_ttl_does_not_survive_restart() {
    let _guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    // Isolate HOME so the daemon writes its socket/db/lock under tmpdir.
    std::env::set_var("HOME", tmp.path());
    let port = pick_free_port();
    let cfg = Config {
        port,
        bind: "127.0.0.1".into(),
        ..Config::default()
    };

    // ---- Boot daemon #1 ----
    let shutdown1 = CancellationToken::new();
    let cfg_d1 = cfg.clone();
    let s1 = shutdown1.clone();
    let d1 = tokio::spawn(async move {
        let _ = servant::daemon::run_daemon_async_with_shutdown(cfg_d1, s1).await;
    });

    let sock = cfg.control_socket_path();
    assert!(
        wait_for_socket(&sock, Duration::from_secs(5)).await,
        "daemon #1 socket never appeared"
    );

    // Register a file with --ttl 2s.
    let file = tmp.path().join("a.html");
    std::fs::write(&file, "<h1>a</h1>").unwrap();
    let client = Arc::new(servant::client::ControlClient::new(&cfg));
    let abs = file.canonicalize().unwrap().to_string_lossy().to_string();
    let r = client
        .register(servant::control::RegisterRequest {
            path: abs.clone(),
            kind: "file".into(),
            ttl: TtlRequest::Seconds(2),
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(r.registration.url_path, "/a.html");

    // Hit every 500ms for 1s to exercise the sliding-TTL path.
    let url = format!("http://127.0.0.1:{port}/a.html");
    for _ in 0..2 {
        let resp = reqwest::get(&url).await.unwrap();
        assert!(
            resp.status().is_success(),
            "expected 200 while live, got {}",
            resp.status()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Stop hitting; wait past the TTL.
    tokio::time::sleep(Duration::from_millis(3_000)).await;

    // The serving plane should already see it as gone.
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "expired row should 404 before restart"
    );

    // Trigger graceful shutdown (equivalent to SIGTERM).
    shutdown1.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), d1).await;
    assert!(
        wait_for_socket_gone(&sock, Duration::from_secs(2)).await,
        "socket should be cleaned up on shutdown"
    );

    // ---- Boot daemon #2 (restart) ----
    let shutdown2 = CancellationToken::new();
    let cfg_d2 = cfg.clone();
    let s2 = shutdown2.clone();
    let d2 = tokio::spawn(async move {
        let _ = servant::daemon::run_daemon_async_with_shutdown(cfg_d2, s2).await;
    });
    assert!(
        wait_for_socket(&sock, Duration::from_secs(5)).await,
        "daemon #2 socket never appeared"
    );

    // The expired row must NOT come back — neither the listing nor a GET
    // should resurrect it.
    let rows = client.list().await.unwrap();
    assert!(
        rows.iter().all(|r| r.url_path != "/a.html"),
        "expired registration must not survive restart, got: {rows:?}"
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET after restart must 404 for an expired row"
    );

    shutdown2.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), d2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)]
async fn never_ttl_survives_restart() {
    let _guard = home_lock().lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", tmp.path());
    let port = pick_free_port();
    let cfg = Config {
        port,
        bind: "127.0.0.1".into(),
        ..Config::default()
    };

    let shutdown1 = CancellationToken::new();
    let cfg_d1 = cfg.clone();
    let s1 = shutdown1.clone();
    let d1 = tokio::spawn(async move {
        let _ = servant::daemon::run_daemon_async_with_shutdown(cfg_d1, s1).await;
    });
    let sock = cfg.control_socket_path();
    assert!(wait_for_socket(&sock, Duration::from_secs(5)).await);

    let file = tmp.path().join("keep.html");
    std::fs::write(&file, "<h1>keep</h1>").unwrap();
    let client = servant::client::ControlClient::new(&cfg);
    let abs = file.canonicalize().unwrap().to_string_lossy().to_string();
    let _ = client
        .register(servant::control::RegisterRequest {
            path: abs.clone(),
            kind: "file".into(),
            ttl: TtlRequest::Never,
            name: None,
        })
        .await
        .unwrap();

    shutdown1.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), d1).await;
    assert!(wait_for_socket_gone(&sock, Duration::from_secs(2)).await);

    // Restart.
    let shutdown2 = CancellationToken::new();
    let cfg_d2 = cfg.clone();
    let s2 = shutdown2.clone();
    let d2 = tokio::spawn(async move {
        let _ = servant::daemon::run_daemon_async_with_shutdown(cfg_d2, s2).await;
    });
    assert!(wait_for_socket(&sock, Duration::from_secs(5)).await);

    let rows = client.list().await.unwrap();
    assert!(
        rows.iter().any(|r| r.url_path == "/keep.html"),
        "--ttl never registration must survive restart, got: {rows:?}"
    );
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/keep.html"))
        .await
        .unwrap();
    assert!(resp.status().is_success());

    shutdown2.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), d2).await;
}
