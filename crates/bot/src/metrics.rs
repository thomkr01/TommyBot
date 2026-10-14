//! Prometheus text metrics on `METRICS_ADDR` (off when unset; bind it to localhost or a private network).
use crate::app::{now_secs, App};
use anyhow::Result;
use std::sync::{
    atomic::{AtomicU64, Ordering::Relaxed},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub static EVENTS: AtomicU64 = AtomicU64::new(0);
pub static EVENT_ERRORS: AtomicU64 = AtomicU64::new(0);
pub static GATEWAY_ERRORS: AtomicU64 = AtomicU64::new(0);
pub static JOBS_DONE: AtomicU64 = AtomicU64::new(0);
pub static JOBS_FAILED: AtomicU64 = AtomicU64::new(0);
/// Event handlers running right now; shutdown waits for it to reach zero.
pub static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

fn render(gauges: &[(&str, u64)]) -> String {
    let counters = [
        ("events_total", &EVENTS),
        ("event_errors_total", &EVENT_ERRORS),
        ("gateway_errors_total", &GATEWAY_ERRORS),
        ("jobs_done_total", &JOBS_DONE),
        ("jobs_failed_total", &JOBS_FAILED),
    ];
    let mut out = String::new();
    for (name, v) in counters {
        out.push_str(&format!(
            "# TYPE tommybot_{name} counter\ntommybot_{name} {}\n",
            v.load(Relaxed)
        ));
    }
    for (name, v) in gauges {
        out.push_str(&format!(
            "# TYPE tommybot_{name} gauge\ntommybot_{name} {v}\n"
        ));
    }
    out
}

async fn snapshot(app: &App) -> String {
    let backlog = tommybot_core::jobs::backlog(&app.pool).await.unwrap_or(0);
    let now_ms = now_secs() * 1000;
    let raids = app
        .raids
        .lock()
        .unwrap()
        .values()
        .filter(|r| r.active(now_ms))
        .count();
    render(&[
        ("jobs_backlog", backlog as u64),
        ("panic_active", app.panic_runs.lock().unwrap().len() as u64),
        ("raids_active", raids as u64),
        ("events_in_flight", IN_FLIGHT.load(Relaxed)),
    ])
}

/// Answers every request on the port with the metrics, whatever the path.
pub async fn serve(app: Arc<App>, addr: String) -> Result<()> {
    let l = TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "metrics listening");
    loop {
        let (mut sock, _) = l.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            // Read the request first so closing with unread bytes does not reset the reply.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sock.read(&mut [0u8; 1024]),
            )
            .await;
            let body = snapshot(&app).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_counters_and_gauges() {
        super::EVENTS.fetch_add(2, super::Relaxed);
        let t = super::render(&[("panic_active", 1)]);
        assert!(t.contains("# TYPE tommybot_events_total counter\n"));
        assert!(t.contains("tommybot_panic_active 1\n"));
    }
}
