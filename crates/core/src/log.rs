//! Logs to stdout, and also to hourly files in `LOG_DIR` when set, so the MCP server can read them.
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Keep the returned guard alive in `main`, or the last lines are not flushed to the file.
pub fn init(name: &str, filter: EnvFilter) -> Option<WorkerGuard> {
    // shortcut: rotates by time, not size (24 x 1 h); add a size cap if a log storm ever fills the disk.
    let file = std::env::var("LOG_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .and_then(|dir| {
            Builder::new()
                .rotation(Rotation::HOURLY)
                .filename_prefix(name)
                .filename_suffix("log")
                .max_log_files(24)
                .build(&dir)
                .map_err(|e| eprintln!("file logging off, cannot write to {dir}: {e}"))
                .ok()
        })
        .map(tracing_appender::non_blocking);
    let (layer, guard) = match file {
        Some((w, g)) => (Some(fmt::layer().with_ansi(false).with_writer(w)), Some(g)),
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(layer)
        .init();
    guard
}
