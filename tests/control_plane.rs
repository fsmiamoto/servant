//! M2 verify: spin a daemon up with HOME=tmpdir and confirm serve/ls/rm
//! round-trip + idempotency.

use std::sync::Arc;
use std::time::{Duration, Instant};

use servant::config::Config;
use servant::control::TtlRequest;

fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn wait_for_socket(p: &std::path::Path, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if p.exists() {
            // Also try to actually connect.
            if tokio::net::UnixStream::connect(p).await.is_ok() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn serve_ls_rm_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", tmp.path());
    let port = pick_free_port();
    let cfg = Config {
        port,
        bind: "127.0.0.1".into(),
        ..Config::default()
    };

    let cfg_for_daemon = cfg.clone();
    let daemon_handle = tokio::spawn(async move {
        let _ = servant::daemon::run_daemon_async(cfg_for_daemon).await;
    });

    // Wait for the socket.
    let sock = cfg.control_socket_path();
    assert!(
        wait_for_socket(&sock, Duration::from_secs(5)).await,
        "daemon socket never appeared"
    );

    // Create a file to register.
    let file = tmp.path().join("a.html");
    std::fs::write(&file, "<h1>a</h1>").unwrap();

    let client = Arc::new(servant::client::ControlClient::new(&cfg));

    let req = servant::control::RegisterRequest {
        path: file.canonicalize().unwrap().to_string_lossy().into(),
        kind: "file".into(),
        ttl: TtlRequest::Default,
        name: None,
    };
    let r1 = client.register(req).await.unwrap();
    assert_eq!(r1.registration.url_path, "/a.html");
    assert!(!r1.reused);

    // Re-register → reused.
    let req2 = servant::control::RegisterRequest {
        path: file.canonicalize().unwrap().to_string_lossy().into(),
        kind: "file".into(),
        ttl: TtlRequest::Default,
        name: None,
    };
    let r2 = client.register(req2).await.unwrap();
    assert!(r2.reused, "re-register must be idempotent");
    assert_eq!(r2.registration.url_path, "/a.html");

    // List shows one row.
    let rows = client.list().await.unwrap();
    assert_eq!(rows.len(), 1);

    // GET via TCP.
    let body = reqwest::get(format!("http://127.0.0.1:{port}/a.html"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("<h1>a</h1>"));

    // Rm by id.
    let _ = client.rm_by_id(r1.registration.id).await.unwrap();
    let rows = client.list().await.unwrap();
    assert_eq!(rows.len(), 0);

    // Trigger shutdown by sending SIGTERM to our own process via tokio.
    // Simpler: drop everything and abort the daemon.
    daemon_handle.abort();
    let _ = daemon_handle.await;
}
