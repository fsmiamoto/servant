//! TTL parsing, rendering, and the `TouchDebouncer`.
//!
//! TTL strings accept human-friendly durations (`30s`, `5m`, `2h`, `24h`,
//! `7d`) plus the literal `never` (case-insensitive). Unit-less values
//! and zero/negative durations are rejected.
//!
//! `TouchDebouncer` coalesces sliding-TTL writes from the serving plane
//! so a page with 50 embedded assets doesn't cause 50 UPDATEs per
//! refresh.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::db::{self, SharedDb};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    Duration(Duration),
    Never,
}

impl Ttl {
    pub fn as_seconds(&self) -> Option<i64> {
        match self {
            Ttl::Duration(d) => Some(d.as_secs() as i64),
            Ttl::Never => None,
        }
    }

    pub fn render(&self) -> String {
        match self {
            Ttl::Never => "never".into(),
            Ttl::Duration(d) => humantime::format_duration(*d).to_string(),
        }
    }
}

impl FromStr for Ttl {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("--ttl must be > 0 or 'never'".into());
        }
        if trimmed.eq_ignore_ascii_case("never") {
            return Ok(Ttl::Never);
        }
        // Reject pure-numeric (unit-less) values to avoid 60-what ambiguity.
        if trimmed.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Err("--ttl requires an explicit unit (e.g. 30s, 5m, 2h)".into());
        }
        if trimmed.starts_with('-') {
            return Err("--ttl must be > 0 or 'never'".into());
        }
        let d = humantime::parse_duration(trimmed)
            .map_err(|e| format!("could not parse --ttl: {e}"))?;
        if d.as_secs() == 0 {
            return Err("--ttl must be > 0 or 'never'".into());
        }
        Ok(Ttl::Duration(d))
    }
}

// Serde for config files: stored as a string in TOML.
impl Serialize for Ttl {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.render())
    }
}

impl<'de> Deserialize<'de> for Ttl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ttl::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Format a positive duration as "in 23h 45m" with at most two units, or
/// `"never"` if input is None.
pub fn format_expires_in(secs: Option<i64>) -> String {
    match secs {
        None => "never".into(),
        Some(s) if s <= 0 => "expired".into(),
        Some(s) => {
            let d = Duration::from_secs(s as u64);
            humantime::format_duration(coarsen(d)).to_string()
        }
    }
}

fn coarsen(d: Duration) -> Duration {
    // Drop sub-second precision for human display.
    let s = d.as_secs();
    if s >= 3600 {
        // round to nearest minute
        Duration::from_secs((s / 60) * 60)
    } else if s >= 60 {
        Duration::from_secs(s)
    } else {
        Duration::from_secs(s.max(1))
    }
}

// ---------------------------------------------------------------------------
// TouchDebouncer
// ---------------------------------------------------------------------------

const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_secs(30);

enum TouchMsg {
    Touch { id: i64, ttl_seconds: Option<i64> },
    Flush(oneshot::Sender<()>),
}

pub struct TouchDebouncer {
    tx: mpsc::UnboundedSender<TouchMsg>,
}

impl TouchDebouncer {
    /// Spawn the background task and return a handle. Pass the same
    /// `Arc<Mutex<Connection>>` you use elsewhere.
    pub fn spawn(db: SharedDb) -> Arc<Self> {
        Self::spawn_with(db, DEFAULT_COALESCE_WINDOW)
    }

    pub fn spawn_with(db: SharedDb, window: Duration) -> Arc<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let pending: Arc<AsyncMutex<HashMap<i64, Instant>>> = Default::default();
        let pending2 = pending.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    TouchMsg::Touch { id, ttl_seconds } => {
                        let Some(ttl_seconds) = ttl_seconds else {
                            // `--ttl never` rows must not generate SQL UPDATEs.
                            continue;
                        };
                        let mut p = pending2.lock().await;
                        let now = Instant::now();
                        let effective_window = coalesce_window(window, ttl_seconds);
                        let should_flush_now = match p.get(&id) {
                            None => true,
                            Some(t) => now.duration_since(*t) >= effective_window,
                        };
                        if should_flush_now {
                            p.insert(id, now);
                            drop(p);
                            flush_one(&db, id);
                        }
                        // Else: within window, suppress. The worst-case
                        // expiry drift is bounded by `effective_window`.
                    }
                    TouchMsg::Flush(reply) => {
                        let mut p = pending2.lock().await;
                        let ids: Vec<i64> = p.keys().copied().collect();
                        p.clear();
                        drop(p);
                        for id in ids {
                            flush_one(&db, id);
                        }
                        let _ = reply.send(());
                    }
                }
            }
        });
        // Spawn a periodic re-flush so stale entries in the map don't
        // grow unbounded.
        let pending_gc = pending.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(window);
            loop {
                tick.tick().await;
                let mut p = pending_gc.lock().await;
                let cutoff = Instant::now() - window;
                p.retain(|_, t| *t >= cutoff);
            }
        });
        Arc::new(Self { tx })
    }

    /// Non-blocking: enqueue an id; first hit (or first after the
    /// coalesce window) actually writes. `ttl_seconds = None` (`never`)
    /// is intentionally skipped to avoid pointless SQL updates.
    pub fn touch(&self, id: i64, ttl_seconds: Option<i64>) {
        let _ = self.tx.send(TouchMsg::Touch { id, ttl_seconds });
    }

    /// Drain pending touches; used on shutdown.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(TouchMsg::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

fn coalesce_window(default_window: Duration, ttl_seconds: i64) -> Duration {
    // A fixed 30s debounce would let very short-lived registrations expire
    // while actively being hit. Bound the debounce to half the TTL (minimum
    // 1s) so `--ttl 2s` can still slide during tests and real use.
    let ttl = Duration::from_secs(ttl_seconds.max(1) as u64);
    default_window.min((ttl / 2).max(Duration::from_secs(1)))
}

fn flush_one(db: &SharedDb, id: i64) {
    let conn = db.lock().unwrap();
    let now = db::now_unix();
    // Look up ttl_seconds; skip if row vanished or is `never`.
    if let Ok(Some(reg)) = db::find_by_id(&conn, id) {
        if reg.ttl_seconds.is_none() {
            return;
        }
        // Don't resurrect a row that already expired between the request
        // that queued the touch and this flush running. `db::touch` also
        // enforces this in SQL; checking here saves the UPDATE round-trip
        // and keeps the intent obvious to readers.
        if let Some(exp) = reg.expires_at {
            if exp < now {
                return;
            }
        }
        let _ = db::touch(&conn, id, now, reg.ttl_seconds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(
            Ttl::from_str("30s").unwrap(),
            Ttl::Duration(Duration::from_secs(30))
        );
        assert_eq!(
            Ttl::from_str("5m").unwrap(),
            Ttl::Duration(Duration::from_secs(300))
        );
        assert_eq!(
            Ttl::from_str("2h").unwrap(),
            Ttl::Duration(Duration::from_secs(7200))
        );
        assert_eq!(
            Ttl::from_str("24h").unwrap(),
            Ttl::Duration(Duration::from_secs(86_400))
        );
        assert_eq!(
            Ttl::from_str("7d").unwrap(),
            Ttl::Duration(Duration::from_secs(7 * 86_400))
        );
    }

    #[test]
    fn never_case_insensitive() {
        assert_eq!(Ttl::from_str("never").unwrap(), Ttl::Never);
        assert_eq!(Ttl::from_str("NEVER").unwrap(), Ttl::Never);
        assert_eq!(Ttl::from_str("Never").unwrap(), Ttl::Never);
    }

    #[test]
    fn rejects_zero() {
        assert!(Ttl::from_str("0s").is_err());
        assert!(Ttl::from_str("0m").is_err());
    }

    #[test]
    fn rejects_unitless() {
        assert!(Ttl::from_str("60").is_err());
    }

    #[test]
    fn rejects_negative() {
        assert!(Ttl::from_str("-5s").is_err());
        assert!(Ttl::from_str("-1").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Ttl::from_str("garbage").is_err());
        assert!(Ttl::from_str("").is_err());
        assert!(Ttl::from_str("  ").is_err());
    }
}
