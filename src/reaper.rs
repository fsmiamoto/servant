//! Periodic background sweep: deletes rows whose `expires_at` is in the
//! past. Shares the daemon's connection mutex.

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::{self, SharedDb};

pub fn spawn_reaper(
    db: SharedDb,
    interval: Duration,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Don't fire immediately on startup; prune_on_load already swept.
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::debug!(target: "servant", "reaper shutdown");
                    break;
                }
                _ = tick.tick() => {
                    let n = {
                        let conn = db.lock().unwrap();
                        db::reap_expired(&conn).unwrap_or(0)
                    };
                    if n > 0 {
                        tracing::info!(target: "servant", reaped = n, "reaper swept expired rows");
                    } else {
                        tracing::debug!(target: "servant", "reaper tick: nothing expired");
                    }
                }
            }
        }
    })
}
