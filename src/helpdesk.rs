//! Клиент HTTP/JSON API сайта техподдержки: вход, список новых тикетов, захват.
//!
//! Все эндпоинты и JSON-пути задаются в config.toml: реальный API сайта
//! программе заранее неизвестен. Опрос и захват идут на один хост через один
//! `reqwest::Client`, поэтому keep-alive соединение всегда «тёплое» и при
//! захвате нет TCP/TLS-рукопожатия.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::cookie::{CookieStore, Jar};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::config::{self, AuthMode, HelpdeskCfg};

/// Имя файла с актуальными cookie сессии (лежит рядом с config.toml).
pub const SESSION_FILE: &str = "session.json";

#[derive(Debug, thiserror::Error)]
pub enum HelpdeskError {
    #[error("сетевая ошибка: {0}")]
    Network(String),
    #[error("вход истёк или отклонён сервером (HTTP {0})")]
    AuthExpired(u16),
    #[error("не удалось войти: {0}")]
    Login(String),
    #[error("сервер просит снизить частоту запросов (HTTP 429), пауза {0:?}")]
    RateLimited(Duration),
    #[error("сервер ответил HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("не удалось разобрать ответ: {0}")]
    Parse(String),
    #[error("ошибка конфигурации: {0}")]
    Config(String),
}

impl HelpdeskError {
    /// Ошибки, после которых повторять вход сразу бессмысленно и опасно
    /// (можно получить блокировку учётной записи).
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::AuthExpired(_) | Self::Login(_))
    }
}

fn net_err(e: reqwest::Error) -> HelpdeskError {
    let kind = if e.is_timeout() {
        "таймаут"
    } else if e.is_connect() {
        "нет соединения"
    } else {
        "ошибка запроса"
    };
    HelpdeskError::Network(format!("{kind}: {e}"))
}

#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    /// Пары «поле: значение» из `extra_fields`.
    pub extra: Vec<(String, String)>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// Сервер подтвердил захват (2xx). `rtt` — время запроса захвата.
    Claimed { rtt: Duration },
    /// Тикет уже взял кто-то другой (404/409/410/422).
    AlreadyTaken { status: u16 },
    Failed(String),
}

pub struct Helpdesk {
    http: Client,
    cfg: HelpdeskCfg,
    /// Хранилище cookie клиента: сюда засевается вставленный Cookie и сюда же
    /// попадают Set-Cookie от сервера.
    jar: Arc<Jar>,
    /// URL списка тикетов: по нему берутся cookie для сохранения в session.json.
    cookie_url: Option<Url>,
    /// Куда сохранять обновлённые сервером cookie (None: не сохранять).
    session_file: Option<PathBuf>,
    /// Последняя сохранённая строка cookie: файл пишется только при изменении.
    last_saved: std::sync::Mutex<String>,
    /// Готовое значение заголовка авторизации (None: cookie-сессия или ещё не вошли).
    auth: RwLock<Option<String>>,
    logged_in: AtomicBool,
    /// Номер «поколения» входа: чтобы несколько параллельных запросов, получивших
    /// 401, выполнили повторный вход только один раз.
    login_epoch: AtomicU64,
    login_lock: Mutex<()>,
    list_method: Method,
    claim_method: Method,
    refresh_method: Method,
}

/// Значение JSON как строка: строки без кавычек, числа и прочее как JSON.
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn pointer<'a>(v: &'a Value, p: &str) -> Option<&'a Value> {
    if p.is_empty() { Some(v) } else { v.pointer(p) }
}

/// Кодирует id для подстановки в путь URL.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Экранирует строку для подстановки внутрь JSON-строки шаблона.
fn json_escape_inner(s: &str) -> String {
    let quoted = Value::String(s.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        out.push('…');
    }
    out
}

/// Разбирает строку Cookie из браузера (`a=1; b=2`, можно с префиксом `Cookie:`)
/// на пары имя=значение. Пустые куски и куски без `=` пропускаются, значения
/// не декодируются.
pub fn parse_cookie_pairs(s: &str) -> Vec<(String, String)> {
    let mut s = s.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if s.len() >= 7 && s.is_char_boundary(7) && s[..7].eq_ignore_ascii_case("cookie:") {
        s = &s[7..];
    }
    s.split([';', '\n', '\r'])
        .filter_map(|part| {
            let (k, v) = part.trim().split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Стабильный (не зависящий от версии Rust) отпечаток вставленного токена:
/// FNV-1a 64. Не криптография: нужен только чтобы заметить, что пользователь
/// вставил через setup новый Cookie и старый session.json больше не годится.
pub fn fingerprint(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.trim().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SessionFile {
    source: u64,
    cookie: String,
}

fn retry_after(resp: &Response) -> Duration {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30))
        .min(Duration::from_secs(15 * 60))
}

impl Helpdesk {
    pub fn new(cfg: HelpdeskCfg, request_timeout: Duration) -> Result<Self, HelpdeskError> {
        crate::init_tls();
        let jar = Arc::new(Jar::default());
        let http = Client::builder()
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .connect_timeout(request_timeout)
            .timeout(request_timeout)
            .cookie_provider(Arc::clone(&jar))
            .user_agent(concat!("autoticket/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| HelpdeskError::Config(format!("HTTP-клиент: {e}")))?;
        let parse_method = |m: &str| {
            Method::from_bytes(m.trim().to_ascii_uppercase().as_bytes())
                .map_err(|_| HelpdeskError::Config(format!("неизвестный HTTP-метод {m}")))
        };
        let base = cfg.base_url.trim().trim_end_matches('/');
        let list = cfg.list_path.trim();
        let cookie_url = if list.starts_with("http://") || list.starts_with("https://") {
            Url::parse(list)
        } else {
            Url::parse(&format!("{base}/{}", list.trim_start_matches('/')))
        }
        .or_else(|_| Url::parse(base))
        .ok();
        Ok(Self {
            list_method: parse_method(&cfg.list_method)?,
            claim_method: parse_method(&cfg.claim_method)?,
            refresh_method: parse_method(&cfg.refresh_method)?,
            http,
            jar,
            cookie_url,
            session_file: None,
            last_saved: std::sync::Mutex::new(String::new()),
            cfg,
            auth: RwLock::new(None),
            logged_in: AtomicBool::new(false),
            login_epoch: AtomicU64::new(0),
            login_lock: Mutex::new(()),
        })
    }

    /// Сохранять обновлённые сервером cookie в этот файл (обычно
    /// `<папка конфига>/session.json`) и брать их оттуда после перезапуска.
    pub fn with_session_file(mut self, path: PathBuf) -> Self {
        self.session_file = Some(path);
        self
    }

    pub fn cfg(&self) -> &HelpdeskCfg {
        &self.cfg
    }

    /// Режим «вставленный Cookie»: сессия живёт в cookie Jar, а не в заголовке.
    pub fn cookie_mode(&self) -> bool {
        self.cfg.auth_mode == AuthMode::Token && self.cfg.auth_header.trim().eq_ignore_ascii_case("cookie")
    }

    /// Строка cookie для засева: из session.json, если он создан из того же
    /// вставленного значения, иначе из config.toml.
    fn cookie_source(&self) -> String {
        let fp = fingerprint(&self.cfg.token);
        if let Some(path) = &self.session_file
            && let Ok(text) = std::fs::read_to_string(path)
        {
            match serde_json::from_str::<SessionFile>(&text) {
                Ok(sf) if sf.source == fp && !sf.cookie.trim().is_empty() => {
                    info!("cookie сессии взяты из {}", path.display());
                    *self.last_saved.lock().unwrap_or_else(|e| e.into_inner()) = sf.cookie.clone();
                    return sf.cookie;
                }
                Ok(_) => info!("в config.toml новый Cookie: старый {} не используется", path.display()),
                Err(e) => warn!("{} повреждён ({e}), беру Cookie из config.toml", path.display()),
            }
        }
        self.cfg.token.clone()
    }

    fn seed_cookies(&self) -> Result<(), HelpdeskError> {
        let base = Url::parse(self.cfg.base_url.trim())
            .map_err(|e| HelpdeskError::Config(format!("base_url не URL: {e}")))?;
        let pairs = parse_cookie_pairs(&self.cookie_source());
        if pairs.is_empty() {
            return Err(HelpdeskError::Config(
                "в helpdesk.token нет ни одной пары имя=значение для Cookie".into(),
            ));
        }
        for (k, v) in &pairs {
            self.jar.add_cookie_str(&format!("{k}={v}; Path=/"), &base);
        }
        debug!("засеяно cookie: {}", pairs.len());
        Ok(())
    }

    /// Сохраняет текущие cookie в session.json, если они изменились. Только в
    /// режиме Cookie и только вне пути захвата. Ошибки записи пишутся в лог.
    pub fn persist_session(&self) {
        if !self.cookie_mode() {
            return;
        }
        let (Some(path), Some(url)) = (&self.session_file, &self.cookie_url) else { return };
        let Some(current) = self.jar.cookies(url).and_then(|v| v.to_str().ok().map(str::to_string)) else {
            return;
        };
        let mut last = self.last_saved.lock().unwrap_or_else(|e| e.into_inner());
        if *last == current {
            return;
        }
        let sf = SessionFile { source: fingerprint(&self.cfg.token), cookie: current.clone() };
        let bytes = match serde_json::to_vec_pretty(&sf) {
            Ok(b) => b,
            Err(e) => {
                warn!("не удалось подготовить {SESSION_FILE}: {e}");
                return;
            }
        };
        match config::write_private(path, &bytes) {
            Ok(()) => {
                if last.is_empty() {
                    info!("cookie сессии сохранены в {}", path.display());
                } else {
                    info!("сервер обновил cookie сессии, сохранено в {SESSION_FILE}");
                }
                *last = current;
            }
            Err(e) => warn!("не удалось сохранить {}: {e:#}", path.display()),
        }
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            let base = self.cfg.base_url.trim_end_matches('/');
            if path.starts_with('/') {
                format!("{base}{path}")
            } else {
                format!("{base}/{path}")
            }
        }
    }

    fn header_value(&self, token: &str) -> String {
        let scheme = self.cfg.auth_scheme.trim();
        if scheme.is_empty() {
            token.to_string()
        } else {
            format!("{scheme} {token}")
        }
    }

    /// Забыть текущий вход: следующий запрос сначала войдёт заново.
    pub fn invalidate(&self) {
        self.logged_in.store(false, Ordering::SeqCst);
    }

    /// Вход. В режиме token просто берёт токен из конфига.
    pub async fn login(&self) -> Result<(), HelpdeskError> {
        let header = match self.cfg.auth_mode {
            AuthMode::Token if self.cookie_mode() => {
                self.seed_cookies()?;
                None
            }
            AuthMode::Token => Some(self.header_value(self.cfg.token.trim())),
            AuthMode::Password => self.password_login().await?,
        };
        *self.auth.write().await = header;
        self.logged_in.store(true, Ordering::SeqCst);
        self.login_epoch.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn password_login(&self) -> Result<Option<String>, HelpdeskError> {
        let c = &self.cfg;
        let mut body = serde_json::Map::new();
        body.insert(c.login_username_field.clone(), Value::String(c.username.clone()));
        body.insert(c.login_password_field.clone(), Value::String(c.password.clone()));
        let mut rb = self.http.post(self.url(&c.login_path)).json(&Value::Object(body));
        for (k, v) in &c.extra_headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await.map_err(net_err)?;
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(HelpdeskError::RateLimited(retry_after(&resp)));
        }
        let text = resp.text().await.map_err(net_err)?;
        if status.is_server_error() {
            return Err(HelpdeskError::Http { status: status.as_u16(), body: snippet(&text) });
        }
        if !status.is_success() {
            return Err(HelpdeskError::Login(format!(
                "HTTP {}: {}",
                status.as_u16(),
                snippet(&text)
            )));
        }
        if c.token_pointer.is_empty() {
            info!("вход выполнен (сессия через cookie)");
            return Ok(None);
        }
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| HelpdeskError::Login(format!("ответ входа не JSON: {e}")))?;
        let token = json
            .pointer(&c.token_pointer)
            .map(value_to_string)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                HelpdeskError::Login(format!("в ответе входа нет токена по пути {}", c.token_pointer))
            })?;
        info!("вход выполнен (токен получен)");
        Ok(Some(self.header_value(&token)))
    }

    async fn ensure_login(&self) -> Result<(), HelpdeskError> {
        if self.logged_in.load(Ordering::SeqCst) {
            return Ok(());
        }
        let _g = self.login_lock.lock().await;
        if self.logged_in.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.login().await
    }

    /// Повторный вход после 401, если с момента `seen_epoch` никто ещё не вошёл заново.
    async fn relogin(&self, seen_epoch: u64) -> Result<(), HelpdeskError> {
        let _g = self.login_lock.lock().await;
        if self.login_epoch.load(Ordering::SeqCst) != seen_epoch {
            return Ok(());
        }
        info!("сервер ответил 401/403, выполняю повторный вход");
        self.logged_in.store(false, Ordering::SeqCst);
        self.login().await
    }

    /// Обновление сессии через refresh_path после 401/403 (режим token). Если
    /// с момента `seen_epoch` сессию уже обновила другая задача, ничего не делает.
    async fn refresh(&self, seen_epoch: u64) -> Result<(), HelpdeskError> {
        let _g = self.login_lock.lock().await;
        if self.login_epoch.load(Ordering::SeqCst) != seen_epoch {
            return Ok(());
        }
        info!("сервер ответил 401/403, обновляю сессию через refresh");
        let c = &self.cfg;
        let mut rb = self.http.request(self.refresh_method.clone(), self.url(&c.refresh_path));
        let body = c.refresh_body.trim();
        if !body.is_empty() {
            rb = rb.header(reqwest::header::CONTENT_TYPE, "application/json").body(body.to_string());
        }
        let rb = self.apply_auth(rb).await;
        let resp = rb.send().await.map_err(net_err)?;
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(HelpdeskError::RateLimited(retry_after(&resp)));
        }
        let text = resp.text().await.map_err(net_err)?;
        if status.is_server_error() {
            return Err(HelpdeskError::Http { status: status.as_u16(), body: snippet(&text) });
        }
        if !status.is_success() {
            warn!("refresh отклонён сервером: HTTP {}", status.as_u16());
            self.invalidate();
            return Err(HelpdeskError::AuthExpired(status.as_u16()));
        }
        if !c.refresh_token_pointer.is_empty() {
            let token = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|j| j.pointer(&c.refresh_token_pointer).map(value_to_string))
                .filter(|t| !t.is_empty());
            let Some(token) = token else {
                warn!("в ответе refresh нет токена по пути {}", c.refresh_token_pointer);
                self.invalidate();
                return Err(HelpdeskError::AuthExpired(status.as_u16()));
            };
            *self.auth.write().await = Some(self.header_value(&token));
        }
        self.login_epoch.fetch_add(1, Ordering::SeqCst);
        info!("сессия обновлена через refresh");
        self.persist_session();
        Ok(())
    }

    async fn apply_auth(&self, mut rb: RequestBuilder) -> RequestBuilder {
        for (k, v) in &self.cfg.extra_headers {
            rb = rb.header(k, v);
        }
        if let Some(h) = self.auth.read().await.as_deref() {
            rb = rb.header(self.cfg.auth_header.as_str(), h);
        }
        rb
    }

    /// Отправка с авторизацией: на 401/403 один раз входит заново и повторяет запрос.
    async fn send_authed<F>(&self, build: F) -> Result<Response, HelpdeskError>
    where
        F: Fn(&Client) -> RequestBuilder,
    {
        self.ensure_login().await?;
        for attempt in 0..2 {
            let epoch = self.login_epoch.load(Ordering::SeqCst);
            let rb = self.apply_auth(build(&self.http)).await;
            let resp = rb.send().await.map_err(net_err)?;
            let st = resp.status();
            if st == StatusCode::UNAUTHORIZED || st == StatusCode::FORBIDDEN {
                if attempt == 0 && self.cfg.auth_mode == AuthMode::Password {
                    self.relogin(epoch).await?;
                    continue;
                }
                if attempt == 0 && self.cfg.auth_mode == AuthMode::Token && !self.cfg.refresh_path.trim().is_empty() {
                    self.refresh(epoch).await?;
                    continue;
                }
                self.invalidate();
                return Err(HelpdeskError::AuthExpired(st.as_u16()));
            }
            if st == StatusCode::TOO_MANY_REQUESTS {
                return Err(HelpdeskError::RateLimited(retry_after(&resp)));
            }
            return Ok(resp);
        }
        unreachable!("цикл возвращает результат на второй итерации")
    }

    /// Список новых тикетов (без захвата).
    pub async fn fetch_new(&self) -> Result<Vec<Ticket>, HelpdeskError> {
        let url = self.url(&self.cfg.list_path);
        let body = self.cfg.list_body.trim().to_string();
        let resp = self
            .send_authed(|c| {
                let rb = c.request(self.list_method.clone(), &url);
                if body.is_empty() {
                    rb
                } else {
                    rb.header(reqwest::header::CONTENT_TYPE, "application/json").body(body.clone())
                }
            })
            .await?;
        let status = resp.status();
        let text = resp.text().await.map_err(net_err)?;
        if !status.is_success() {
            return Err(HelpdeskError::Http { status: status.as_u16(), body: snippet(&text) });
        }
        // Сессия жива: сохраняем cookie, если сервер их обновил (не на пути захвата).
        self.persist_session();
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| HelpdeskError::Parse(format!("список тикетов не JSON: {e}")))?;
        self.parse_tickets(&json)
    }

    pub fn parse_tickets(&self, json: &Value) -> Result<Vec<Ticket>, HelpdeskError> {
        let c = &self.cfg;
        let items = pointer(json, &c.list_items_pointer)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                HelpdeskError::Parse(format!(
                    "по пути list_items_pointer = \"{}\" нет массива тикетов",
                    c.list_items_pointer
                ))
            })?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if !c.filter_pointer.is_empty() {
                let v = item.pointer(&c.filter_pointer).map(value_to_string).unwrap_or_default();
                if !c.filter_values.iter().any(|f| f == &v) {
                    continue;
                }
            }
            let Some(id) = item.pointer(&c.id_pointer).map(value_to_string).filter(|s| !s.is_empty())
            else {
                debug!("элемент без id по пути {}: пропускаю", c.id_pointer);
                continue;
            };
            let title = item
                .pointer(&c.title_pointer)
                .map(value_to_string)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "(без темы)".into());
            let extra = c
                .extra_fields
                .iter()
                .filter_map(|p| {
                    let v = item.pointer(p)?;
                    if v.is_null() {
                        return None;
                    }
                    let key = p.rsplit('/').next().unwrap_or(p).to_string();
                    Some((key, value_to_string(v)))
                })
                .collect();
            out.push(Ticket { id, title, extra, raw: item.clone() });
        }
        Ok(out)
    }

    /// Захват тикета. Время `rtt` — от отправки запроса до получения статуса.
    pub async fn claim(&self, t: &Ticket) -> Result<ClaimOutcome, HelpdeskError> {
        let c = &self.cfg;
        let url = self.url(&c.claim_path.replace("{id}", &encode_path_segment(&t.id)));
        let body = c
            .claim_body
            .trim()
            .replace("{id}", &json_escape_inner(&t.id))
            .replace("{username}", &json_escape_inner(&c.username));
        let t0 = Instant::now();
        let resp = self
            .send_authed(|cl| {
                let rb = cl.request(self.claim_method.clone(), &url);
                if body.is_empty() {
                    rb
                } else {
                    rb.header(reqwest::header::CONTENT_TYPE, "application/json").body(body.clone())
                }
            })
            .await?;
        let rtt = t0.elapsed();
        let status = resp.status();
        if status.is_success() {
            // Дочитываем тело, чтобы соединение вернулось в пул.
            let _ = resp.bytes().await;
            return Ok(ClaimOutcome::Claimed { rtt });
        }
        let text = resp.text().await.unwrap_or_default();
        Ok(match status.as_u16() {
            404 | 409 | 410 | 422 => ClaimOutcome::AlreadyTaken { status: status.as_u16() },
            s => ClaimOutcome::Failed(format!("HTTP {s}: {}", snippet(&text))),
        })
    }

    /// Один авторизованный запрос списка (без захвата) для `bench`: время запроса.
    pub async fn probe(&self) -> Result<Duration, HelpdeskError> {
        self.ensure_login().await?;
        let url = self.url(&self.cfg.list_path);
        let t0 = Instant::now();
        let resp = self.send_authed(|c| c.request(self.list_method.clone(), &url)).await?;
        let el = t0.elapsed();
        let ok = resp.status().is_success();
        let _ = resp.bytes().await;
        if ok {
            self.persist_session();
        }
        Ok(el)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_strings() {
        let p = parse_cookie_pairs;
        let kv = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(p("a=1; b=2"), vec![kv("a", "1"), kv("b", "2")]);
        assert_eq!(p("Cookie: a=1;b=2;"), vec![kv("a", "1"), kv("b", "2")]);
        assert_eq!(p("cookie:sid=x=y"), vec![kv("sid", "x=y")]);
        assert_eq!(p(" ; x"), vec![]);
        assert_eq!(p("\"a=1; =2\"\n"), vec![kv("a", "1")]);
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fingerprint(" a=1 "), fingerprint("a=1"));
        assert_ne!(fingerprint("a=1"), fingerprint("a=2"));
    }

    #[test]
    fn encodes_ids() {
        assert_eq!(encode_path_segment("42"), "42");
        assert_eq!(encode_path_segment("a b/c"), "a%20b%2Fc");
        assert_eq!(json_escape_inner("x\"y"), "x\\\"y");
    }

    #[test]
    fn parses_numbers_and_strings() {
        let mut cfg = HelpdeskCfg { base_url: "http://x".into(), ..Default::default() };
        cfg.list_items_pointer = "/data/items".into();
        let hd = Helpdesk::new(cfg, Duration::from_secs(1)).unwrap();
        let v: Value = serde_json::json!({"data":{"items":[
            {"id": 7, "subject": "A", "priority": "high"},
            {"id": "x-1"},
            {"subject": "без id"}
        ]}});
        let t = hd.parse_tickets(&v).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].id, "7");
        assert_eq!(t[0].extra, vec![("priority".to_string(), "high".to_string())]);
        assert_eq!(t[1].title, "(без темы)");
    }
}
