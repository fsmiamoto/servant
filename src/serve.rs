//! Serving plane — TCP, GET/HEAD/OPTIONS only. Looks the request path up
//! in the registry on every hit, dispatches to file or directory serving,
//! and slides the TTL on every 2xx response.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tower_http::trace::TraceLayer;
use tracing::Span;

use crate::db::{self, Kind, Registration, SharedDb};
use crate::guards::{resolve_under_mount, GuardOutcome};
use crate::ttl::TouchDebouncer;

#[derive(Clone)]
pub struct ServeState {
    pub db: SharedDb,
    pub touch: Arc<TouchDebouncer>,
}

pub fn router(state: ServeState) -> Router {
    let trace = TraceLayer::new_for_http()
        .make_span_with(|request: &Request<Body>| {
            tracing::info_span!(
                target: "servant::access",
                "serving_request",
                remote_addr = "-",
                method = %request.method(),
                uri = %request.uri(),
                version = ?request.version(),
                user_agent = request
                    .headers()
                    .get(header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-"),
                referer = request
                    .headers()
                    .get(header::REFERER)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-"),
            )
        })
        .on_response(|response: &Response, latency: Duration, span: &Span| {
            let status = response.status().as_u16();
            let bytes = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            tracing::info!(
                target: "servant::access",
                parent: span,
                status,
                latency_ms = latency.as_millis() as u64,
                bytes,
                "access"
            );
        });

    Router::new()
        .fallback(fallback)
        .layer(trace)
        .with_state(state)
}

async fn fallback(State(s): State<ServeState>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let request_path = req.uri().path().to_string();

    // CORS preflight for any path.
    if method == Method::OPTIONS {
        return cors_preflight();
    }
    if method != Method::GET && method != Method::HEAD {
        return method_not_allowed();
    }

    let lookup = {
        let conn = s.db.lock().unwrap();
        db::find_for_request(&conn, &request_path)
    };
    let matched = match lookup {
        Ok(Some(m)) => m,
        Ok(None) => return not_found_html(&request_path),
        Err(_) => return server_error(),
    };

    let registration = matched.registration.clone();

    let response = match (&registration.kind, matched.suffix) {
        (Kind::File, _) => serve_file(&s, &registration, req).await,
        (Kind::Dir, Some(suffix)) => serve_dir(&s, &registration, &suffix, req).await,
        (Kind::Dir, None) => serve_dir(&s, &registration, "", req).await,
    };

    // Slide TTL on success.
    let status = response.status();
    if status.is_success() {
        // Clear missing flag if previously marked.
        if registration.missing_since.is_some() {
            let conn = s.db.lock().unwrap();
            let _ = db::clear_missing(&conn, registration.id);
        }
        s.touch.touch(registration.id, registration.ttl_seconds);
    } else if status == StatusCode::GONE {
        let now = db::now_unix();
        let conn = s.db.lock().unwrap();
        let _ = db::mark_missing(&conn, registration.id, now);
    }
    apply_headers(response)
}

fn apply_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    insert_default(h, header::CACHE_CONTROL, "no-cache");
    insert_default(h, header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    insert_default(
        h,
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, HEAD, OPTIONS",
    );
    insert_default(h, header::ACCEPT_RANGES, "bytes");
    resp
}

fn insert_default(h: &mut HeaderMap, name: header::HeaderName, value: &'static str) {
    if !h.contains_key(&name) {
        h.insert(name, HeaderValue::from_static(value));
    }
}

fn cors_preflight() -> Response {
    let mut resp = (StatusCode::NO_CONTENT, "").into_response();
    let h = resp.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    resp
}

fn method_not_allowed() -> Response {
    let mut resp = (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response();
    resp.headers_mut().insert(
        header::ALLOW,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    resp
}

fn not_found_html(request_path: &str) -> Response {
    let body = format!(
        "<h1>404 Not Found</h1><p>servant: no registration for <code>{}</code></p>",
        html_escape(request_path)
    );
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn gone_html(reg: &Registration) -> Response {
    let body = format!(
        "<h1>410 Gone</h1><p>servant: source for <code>{}</code> is no longer available at <code>{}</code>.</p>",
        html_escape(&reg.url_path),
        html_escape(&reg.source_path),
    );
    (
        StatusCode::GONE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn server_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

async fn serve_file(_s: &ServeState, reg: &Registration, req: Request<Body>) -> Response {
    let src = reg.source_pathbuf();
    if tokio::fs::metadata(&src).await.is_err() {
        return gone_html(reg);
    }
    let svc = ServeFile::new(&src);
    match svc.oneshot(req).await {
        Ok(resp) => resp.into_response(),
        Err(_) => server_error(),
    }
}

async fn serve_dir(
    _s: &ServeState,
    reg: &Registration,
    suffix: &str,
    req: Request<Body>,
) -> Response {
    let mount = reg.source_pathbuf();
    // Mount root missing → 410.
    if tokio::fs::metadata(&mount).await.is_err() {
        return gone_html(reg);
    }

    let outcome = resolve_under_mount(&mount, suffix);
    let target = match outcome {
        GuardOutcome::Allow(p) => p,
        GuardOutcome::NotFound => return not_found_html(&format!("{}{}", reg.url_path, suffix)),
        GuardOutcome::Forbidden => return forbidden(),
    };

    let md = match tokio::fs::metadata(&target).await {
        Ok(m) => m,
        Err(_) => return not_found_html(&format!("{}{}", reg.url_path, suffix)),
    };

    if md.is_dir() {
        let index = target.join("index.html");
        if tokio::fs::metadata(&index).await.is_ok() {
            return serve_file_at(&index, req).await;
        }
        // Auto-listing.
        let url_prefix = format!("{}{}", reg.url_path, suffix);
        let url_prefix = if url_prefix.ends_with('/') {
            url_prefix
        } else {
            format!("{url_prefix}/")
        };
        return match crate::listing::render(&target, &url_prefix) {
            Ok(html) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response(),
            Err(_) => server_error(),
        };
    }

    serve_file_at(&target, req).await
}

async fn serve_file_at(path: &Path, req: Request<Body>) -> Response {
    let svc = ServeFile::new(path);
    match svc.oneshot(req).await {
        Ok(r) => r.into_response(),
        Err(_) => server_error(),
    }
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden").into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// Trait import shim — `tower::ServiceExt::oneshot` requires `Service` for ServeFile.
#[allow(unused_imports)]
use tower::Service as _;

// PathBuf used in type-checking for clarity in some helpers.
#[allow(dead_code)]
fn _unused_pb(_p: PathBuf) {}
