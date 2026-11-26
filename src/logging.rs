//! Логи: в stdout (видно в journalctl) и в файл `<data_dir>/logs/autoticket.YYYY-MM-DD.log`
//! с ежедневной ротацией (хранится 14 файлов). Уровень задаётся `RUST_LOG`.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_FILTER: &str = "info,hyper=warn,hyper_util=warn,reqwest=warn,rustls=warn";

/// Возвращаемый guard нужно держать до конца `main`, иначе хвост лога потеряется.
pub fn init(log_dir: Option<&Path>, stdout: bool, default_level: Option<&str>) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level.unwrap_or(DEFAULT_FILTER)));

    let mut guard = None;
    let file_layer = log_dir.and_then(|dir| {
        let _ = std::fs::create_dir_all(dir);
        match RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("autoticket")
            .filename_suffix("log")
            .max_log_files(14)
            .build(dir)
        {
            Ok(appender) => {
                let (w, g) = tracing_appender::non_blocking(appender);
                guard = Some(g);
                Some(fmt::layer().with_ansi(false).with_writer(w))
            }
            Err(e) => {
                eprintln!("не удалось открыть лог в {}: {e}", dir.display());
                None
            }
        }
    });
    let stdout_layer = stdout.then(|| fmt::layer().with_writer(std::io::stdout));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init();
    guard
}
