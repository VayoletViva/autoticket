//! Уведомления в Telegram через Bot API.
//!
//! Сообщения ставятся в ограниченную очередь и отправляются фоновой задачей с
//! повторами, так что недоступный бот никогда не тормозит захват тикетов.
//! Токен бота входит в URL запроса, поэтому ошибки reqwest выводятся только
//! через `without_url()`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::config::TelegramCfg;
use crate::helpdesk::Ticket;

pub const QUEUE_CAPACITY: usize = 1000;
pub const ALERT_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const MAX_TEXT: usize = 4000;

#[derive(Debug, thiserror::Error)]
pub enum TgError {
    /// Временная ошибка: стоит повторить (сеть, 5xx, 429).
    #[error("временная ошибка Telegram: {msg}")]
    Retry { msg: String, after: Option<Duration> },
    /// Постоянная ошибка (неверный токен, чат не найден): повтор не поможет.
    #[error("ошибка Telegram: {0}")]
    Fatal(String),
}

#[derive(Clone)]
pub struct TelegramApi {
    http: Client,
    api_base: String,
    token: String,
    pub chat_id: i64,
}

impl TelegramApi {
    pub fn new(cfg: &TelegramCfg, timeout: Duration) -> anyhow::Result<Self> {
        crate::init_tls();
        let http = Client::builder()
            .tcp_nodelay(true)
            .connect_timeout(timeout)
            .timeout(timeout.max(Duration::from_secs(10)))
            .user_agent(concat!("autoticket/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| anyhow::anyhow!("HTTP-клиент: {}", e.without_url()))?;
        Ok(Self {
            http,
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
            token: cfg.bot_token.trim().to_string(),
            chat_id: cfg.chat_id,
        })
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.token, method)
    }

    fn mask(&self, s: &str) -> String {
        if self.token.is_empty() { s.to_string() } else { s.replace(&self.token, "***") }
    }

    async fn call(&self, method: &str, body: Value, timeout: Option<Duration>) -> Result<Value, TgError> {
        let mut rb = self.http.post(self.url(method)).json(&body);
        if let Some(t) = timeout {
            rb = rb.timeout(t);
        }
        let resp = rb.send().await.map_err(|e| TgError::Retry {
            msg: self.mask(&format!("сеть: {}", e.without_url())),
            after: None,
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| TgError::Retry {
            msg: self.mask(&format!("сеть: {}", e.without_url())),
            after: None,
        })?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() && v.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        let desc = v
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| text.chars().take(200).collect());
        let msg = self.mask(&format!("HTTP {}: {}", status.as_u16(), desc));
        let after = v
            .pointer("/parameters/retry_after")
            .and_then(Value::as_u64)
            .map(Duration::from_secs);
        if status.as_u16() == 429 || status.is_server_error() {
            Err(TgError::Retry { msg, after })
        } else {
            Err(TgError::Fatal(msg))
        }
    }

    pub async fn send_message(&self, text: &str) -> Result<(), TgError> {
        self.send_message_to(self.chat_id, text).await
    }

    pub async fn send_message_to(&self, chat_id: i64, text: &str) -> Result<(), TgError> {
        let text = truncate(text, MAX_TEXT);
        self.call(
            "sendMessage",
            json!({"chat_id": chat_id, "text": text, "disable_web_page_preview": true}),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Имя бота (`getMe`), для проверки токена.
    pub async fn get_me(&self) -> Result<String, TgError> {
        let r = self.call("getMe", json!({}), None).await?;
        Ok(r.get("username").and_then(Value::as_str).unwrap_or("?").to_string())
    }

    /// Long polling `getUpdates`.
    pub async fn get_updates(&self, offset: i64, timeout_s: u64) -> Result<Vec<Value>, TgError> {
        let r = self
            .call(
                "getUpdates",
                json!({"offset": offset, "timeout": timeout_s, "allowed_updates": ["message"]}),
                Some(Duration::from_secs(timeout_s + 10)),
            )
            .await?;
        Ok(r.as_array().cloned().unwrap_or_default())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub base: Duration,
    pub max: Duration,
    pub attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { base: Duration::from_secs(1), max: Duration::from_secs(60), attempts: 10 }
    }
}

/// Очередь уведомлений. Клонируется дёшево; `send` никогда не блокирует.
#[derive(Clone)]
pub struct Notifier {
    tx: mpsc::Sender<String>,
    last_alert: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Notifier {
    pub fn spawn(api: TelegramApi, policy: RetryPolicy) -> (Notifier, JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<String>(QUEUE_CAPACITY);
        let handle = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                deliver(&api, &text, policy).await;
            }
        });
        (Notifier { tx, last_alert: Arc::new(Mutex::new(HashMap::new())) }, handle)
    }

    /// Поставить сообщение в очередь. `false`, если очередь переполнена.
    pub fn send(&self, text: String) -> bool {
        match self.tx.try_send(text) {
            Ok(()) => true,
            Err(e) => {
                warn!("очередь уведомлений Telegram недоступна ({e}), сообщение пропущено");
                false
            }
        }
    }

    /// Алерт о длительной проблеме: не чаще одного раза в 30 минут на `kind`.
    pub fn alert(&self, kind: &str, text: &str) -> bool {
        {
            let mut m = self.last_alert.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            if let Some(t) = m.get(kind)
                && now.duration_since(*t) < ALERT_COOLDOWN
            {
                return false;
            }
            m.insert(kind.to_string(), now);
        }
        self.send(format!("⚠️ autoticket: {text}"))
    }
}

async fn deliver(api: &TelegramApi, text: &str, p: RetryPolicy) {
    let mut delay = p.base;
    for attempt in 1..=p.attempts {
        match api.send_message(text).await {
            Ok(()) => {
                if attempt > 1 {
                    info!("уведомление доставлено с попытки {attempt}");
                }
                return;
            }
            Err(TgError::Fatal(msg)) => {
                error!("Telegram отклонил сообщение: {msg}. Проверьте токен бота и chat_id (autoticket setup)");
                return;
            }
            Err(TgError::Retry { msg, after }) => {
                if attempt == p.attempts {
                    error!("Telegram недоступен ({msg}): сообщение не доставлено после {attempt} попыток");
                    return;
                }
                // retry_after от Telegram важнее нашего backoff, но не дольше 10 минут.
                let wait = after.unwrap_or(delay).min(Duration::from_secs(600));
                warn!("Telegram: {msg}; повтор через {:?} (попытка {attempt}/{})", wait, p.attempts);
                tokio::time::sleep(wait).await;
                delay = (delay * 2).min(p.max);
            }
        }
    }
}

/// Текст уведомления о взятом тикете (plain text, без parse_mode).
pub fn format_claim(t: &Ticket, rtt: Duration, detect_to_claim: Duration, ticket_url: &str) -> String {
    let mut s = format!("✅ Взят тикет #{}\n{}\n", t.id, truncate(&t.title, 500));
    for (k, v) in &t.extra {
        s.push_str(&format!("{k}: {}\n", truncate(v, 300)));
    }
    if !ticket_url.trim().is_empty() {
        s.push_str(&ticket_url.trim().replace("{id}", &t.id));
        s.push('\n');
    }
    s.push_str(&format!(
        "Время захвата: {:.1} мс (от обнаружения: {:.1} мс)",
        rtt.as_secs_f64() * 1000.0,
        detect_to_claim.as_secs_f64() * 1000.0
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_message_contains_ticket_data() {
        let t = Ticket {
            id: "42".into(),
            title: "Не работает принтер".into(),
            extra: vec![("priority".into(), "high".into())],
            raw: Value::Null,
        };
        let s = format_claim(&t, Duration::from_micros(8_400), Duration::from_micros(9_100), "https://s/t/{id}");
        assert!(s.contains("#42"));
        assert!(s.contains("Не работает принтер"));
        assert!(s.contains("priority: high"));
        assert!(s.contains("https://s/t/42"));
        assert!(s.contains("8.4 мс"));
    }

    #[test]
    fn truncates_long_text() {
        let s = truncate(&"я".repeat(5000), MAX_TEXT);
        assert_eq!(s.chars().count(), MAX_TEXT + 1);
    }
}
