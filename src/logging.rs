//! Логи: в stdout (видно в journalctl) и в файл `<data_dir>/logs/autoticket.YYYY-MM-DD.log`
//! с ежедневной ротацией (хранится 14 файлов). Уровень задаётся `RUST_LOG`.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_FILTER: &str = "info,hyper=warn,hyper_util=warn,reqwest=warn,rustls=warn";

/// Библиотека `cookie_store` на уровне debug печатает значения cookie (то есть
/// живую сессию). Эта директива добавляется всегда, даже при `RUST_LOG=debug`
/// или `trace`, чтобы сессия не попала в лог.
const SECRET_SAFE_DIRECTIVE: &str = "cookie_store=off";

/// Фильтр логов: `RUST_LOG` (если задан и корректен), иначе уровень по умолчанию,
/// плюс принудительное выключение логов, которые могут содержать cookie.
pub fn build_filter(rust_log: Option<&str>, default_level: Option<&str>) -> EnvFilter {
    let base = rust_log
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| EnvFilter::try_new(s).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level.unwrap_or(DEFAULT_FILTER)));
    base.add_directive(SECRET_SAFE_DIRECTIVE.parse().expect("valid directive"))
}

/// Возвращаемый guard нужно держать до конца `main`, иначе хвост лога потеряется.
pub fn init(log_dir: Option<&Path>, stdout: bool, default_level: Option<&str>) -> Option<WorkerGuard> {
    let filter = build_filter(std::env::var("RUST_LOG").ok().as_deref(), default_level);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_store_is_always_silenced() {
        for env in [None, Some("debug"), Some("trace"), Some("cookie_store=trace"), Some("info")] {
            let f = build_filter(env, None).to_string();
            assert!(f.contains("cookie_store=off"), "{env:?}: {f}");
        }
    }
}
