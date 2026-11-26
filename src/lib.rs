//! autoticket: автоматически берёт новые тикеты техподдержки через HTTP/JSON API
//! сайта и присылает уведомление о каждом взятом тикете в Telegram.

pub mod autostart;
pub mod config;
pub mod helpdesk;
pub mod logging;
pub mod metrics;
pub mod setup;
pub mod telegram;
pub mod worker;

/// Устанавливает криптопровайдер rustls (ring) один раз на процесс.
pub fn init_tls() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
