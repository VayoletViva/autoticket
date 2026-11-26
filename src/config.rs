//! Конфигурация: модель, пути, загрузка и сохранение (на unix с правами 0600).
//!
//! Конфиг с паролями и токенами лежит в системной папке пользователя, а не в
//! репозитории. Путь можно переопределить переменной окружения
//! `AUTOTICKET_CONFIG_DIR`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Минимально допустимый интервал опроса: чаще опрашивать сервер не даём.
pub const MIN_POLL_INTERVAL_MS: u64 = 250;
pub const CONFIG_DIR_ENV: &str = "AUTOTICKET_CONFIG_DIR";
pub const CONFIG_FILE: &str = "config.toml";

#[derive(Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub helpdesk: HelpdeskCfg,
    pub telegram: TelegramCfg,
    pub run: RunCfg,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Вход по логину и паролю; при 401 программа входит заново сама.
    #[default]
    Password,
    /// Готовый токен или cookie, скопированный из браузера.
    Token,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HelpdeskCfg {
    /// Например `https://support.example.com`.
    pub base_url: String,
    pub auth_mode: AuthMode,
    pub username: String,
    pub password: String,
    /// Для `auth_mode = "token"`: значение токена (без схемы) или строка cookie.
    pub token: String,
    pub login_path: String,
    pub login_username_field: String,
    pub login_password_field: String,
    /// JSON Pointer токена в ответе входа. Пусто: сессия держится в cookie.
    pub token_pointer: String,
    pub auth_header: String,
    /// Пусто: значение заголовка = токен как есть (например, для `Cookie`).
    pub auth_scheme: String,
    /// Дополнительные заголовки для всех запросов (например, `X-Requested-With`).
    pub extra_headers: BTreeMap<String, String>,
    pub list_method: String,
    pub list_path: String,
    /// Необязательное тело запроса списка (JSON), если список берётся POST-запросом.
    pub list_body: String,
    /// JSON Pointer массива тикетов. Пусто: корень ответа и есть массив.
    pub list_items_pointer: String,
    /// Необязательный фильтр: учитывать только тикеты, у которых значение по
    /// этому указателю входит в `filter_values` (например, `/status` = `new`).
    pub filter_pointer: String,
    pub filter_values: Vec<String>,
    pub id_pointer: String,
    pub title_pointer: String,
    /// Поля, которые попадут в сообщение Telegram.
    pub extra_fields: Vec<String>,
    pub claim_method: String,
    /// Путь захвата, `{id}` заменяется на id тикета.
    pub claim_path: String,
    /// Необязательный JSON-шаблон тела захвата: `{id}`, `{username}`.
    pub claim_body: String,
    /// Необязательная ссылка на тикет для сообщения: `https://.../tickets/{id}`.
    pub ticket_url: String,
    /// Только для `auth_mode = "token"`: путь обновления сессии (refresh), который
    /// вызывается один раз при ответе 401/403. Пусто: обновление выключено.
    pub refresh_path: String,
    pub refresh_method: String,
    /// Необязательное тело запроса обновления (JSON как есть).
    pub refresh_body: String,
    /// JSON Pointer нового токена в ответе refresh. Пусто: сайт обновляет только cookie.
    pub refresh_token_pointer: String,
}

impl Default for HelpdeskCfg {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            auth_mode: AuthMode::Password,
            username: String::new(),
            password: String::new(),
            token: String::new(),
            login_path: "/api/auth/login".into(),
            login_username_field: "login".into(),
            login_password_field: "password".into(),
            token_pointer: "/token".into(),
            auth_header: "Authorization".into(),
            auth_scheme: "Bearer".into(),
            extra_headers: BTreeMap::new(),
            list_method: "GET".into(),
            list_path: "/api/tickets?status=new".into(),
            list_body: String::new(),
            list_items_pointer: String::new(),
            filter_pointer: String::new(),
            filter_values: Vec::new(),
            id_pointer: "/id".into(),
            title_pointer: "/subject".into(),
            extra_fields: vec!["/priority".into(), "/created_at".into()],
            claim_method: "POST".into(),
            claim_path: "/api/tickets/{id}/assign".into(),
            claim_body: String::new(),
            ticket_url: String::new(),
            refresh_path: String::new(),
            refresh_method: "POST".into(),
            refresh_body: String::new(),
            refresh_token_pointer: "/access_token".into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TelegramCfg {
    pub bot_token: String,
    /// 0 = не задан (будет определён при настройке по сообщению /start).
    pub chat_id: i64,
    pub api_base: String,
}

impl Default for TelegramCfg {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            chat_id: 0,
            api_base: "https://api.telegram.org".into(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default)]
pub struct RunCfg {
    pub poll_interval_ms: u64,
    pub max_backoff_ms: u64,
    pub request_timeout_ms: u64,
}

impl Default for RunCfg {
    fn default() -> Self {
        Self {
            poll_interval_ms: 1000,
            max_backoff_ms: 60_000,
            request_timeout_ms: 5000,
        }
    }
}

fn redact(s: &str) -> &'static str {
    if s.is_empty() { "" } else { "***" }
}

impl fmt::Debug for HelpdeskCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HelpdeskCfg")
            .field("base_url", &self.base_url)
            .field("auth_mode", &self.auth_mode)
            .field("username", &self.username)
            .field("password", &redact(&self.password))
            .field("token", &redact(&self.token))
            .field("login_path", &self.login_path)
            .field("token_pointer", &self.token_pointer)
            .field("auth_header", &self.auth_header)
            .field("extra_headers", &self.extra_headers.keys().collect::<Vec<_>>())
            .field("list_method", &self.list_method)
            .field("list_path", &self.list_path)
            .field("list_items_pointer", &self.list_items_pointer)
            .field("id_pointer", &self.id_pointer)
            .field("title_pointer", &self.title_pointer)
            .field("claim_method", &self.claim_method)
            .field("claim_path", &self.claim_path)
            .field("refresh_path", &self.refresh_path)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for TelegramCfg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramCfg")
            .field("bot_token", &redact(&self.bot_token))
            .field("chat_id", &self.chat_id)
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("helpdesk", &self.helpdesk)
            .field("telegram", &self.telegram)
            .field("run", &self.run)
            .finish()
    }
}

impl Config {
    /// Проверяет обязательные поля и приводит значения к допустимым границам.
    pub fn validate(&mut self) -> Result<()> {
        let h = &mut self.helpdesk;
        h.base_url = h.base_url.trim().trim_end_matches('/').to_string();
        if h.base_url.is_empty() {
            bail!("helpdesk.base_url не задан");
        }
        let u = url::Url::parse(&h.base_url)
            .with_context(|| format!("helpdesk.base_url не похож на URL: {}", h.base_url))?;
        if !matches!(u.scheme(), "http" | "https") {
            bail!("helpdesk.base_url должен начинаться с http:// или https://");
        }
        if !h.claim_path.contains("{id}") {
            bail!("helpdesk.claim_path должен содержать {{id}}");
        }
        for m in [&h.claim_method, &h.list_method, &h.refresh_method] {
            reqwest::Method::from_bytes(m.trim().to_ascii_uppercase().as_bytes())
                .with_context(|| format!("неизвестный HTTP-метод: {m}"))?;
        }
        match h.auth_mode {
            AuthMode::Password if h.username.is_empty() => bail!("helpdesk.username не задан"),
            AuthMode::Token if h.token.is_empty() => bail!("helpdesk.token не задан"),
            _ => {}
        }
        if self.telegram.bot_token.trim().is_empty() {
            bail!("telegram.bot_token не задан");
        }
        if self.telegram.chat_id == 0 {
            bail!("telegram.chat_id не задан: запустите `autoticket setup`");
        }
        let r = &mut self.run;
        r.poll_interval_ms = r.poll_interval_ms.max(MIN_POLL_INTERVAL_MS);
        r.max_backoff_ms = r.max_backoff_ms.max(r.poll_interval_ms);
        r.request_timeout_ms = r.request_timeout_ms.clamp(500, 120_000);
        Ok(())
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "autoticket")
}

/// Папка конфигурации: `$AUTOTICKET_CONFIG_DIR` или системная папка пользователя
/// (Linux `~/.config/autoticket`, Windows `%APPDATA%\autoticket\config`,
/// macOS `~/Library/Application Support/autoticket`).
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(CONFIG_DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    project_dirs()
        .map(|p| p.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".autoticket"))
}

/// Папка для логов и claims.csv: `$AUTOTICKET_CONFIG_DIR` или системная папка данных.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(CONFIG_DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    project_dirs()
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".autoticket"))
}

pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

/// `Ok(None)`, если файла ещё нет (первый запуск).
pub fn load(path: &Path) -> Result<Option<Config>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("не удалось прочитать {}", path.display())),
    };
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("ошибка в файле {}", path.display()))?;
    Ok(Some(cfg))
}

/// Атомарно сохраняет конфиг (временный файл + rename), на unix с правами 0600.
pub fn save(path: &Path, cfg: &Config) -> Result<()> {
    let text = format!(
        "# autoticket: настройки и секреты. Не публикуйте этот файл.\n{}",
        toml::to_string_pretty(cfg)?
    );
    write_private(path, text.as_bytes())
}

/// Атомарная запись файла с секретами: временный файл + rename, на unix
/// папка 0700 и файл 0600. Используется для config.toml и session.json.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("не удалось создать {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("не удалось записать {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("не удалось сохранить {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        let mut c = Config::default();
        c.helpdesk.base_url = "https://support.example.com".into();
        c.helpdesk.username = "agent".into();
        c.helpdesk.password = "s3cr3t-pass".into();
        c.telegram.bot_token = "123456:SECRET-BOT-TOKEN".into();
        c.telegram.chat_id = 111;
        c
    }

    #[test]
    fn save_load_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(CONFIG_FILE);
        let cfg = sample();
        save(&path, &cfg).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert!(back == cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("nope.toml")).unwrap().is_none());
    }

    #[test]
    fn debug_hides_secrets() {
        let s = format!("{:?}", sample());
        assert!(!s.contains("s3cr3t-pass"), "{s}");
        assert!(!s.contains("SECRET-BOT-TOKEN"), "{s}");
        assert!(s.contains("***"));
    }

    #[test]
    fn partial_toml_uses_defaults() {
        let text = r#"
            [helpdesk]
            base_url = "https://support.example.com/"
            username = "agent"
            password = "x"
            [telegram]
            bot_token = "1:A"
            chat_id = 5
        "#;
        let mut cfg: Config = toml::from_str(text).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.helpdesk.base_url, "https://support.example.com");
        assert_eq!(cfg.helpdesk.claim_path, "/api/tickets/{id}/assign");
        assert_eq!(cfg.telegram.api_base, "https://api.telegram.org");
        assert_eq!(cfg.run.poll_interval_ms, 1000);
        // Старый config.toml без полей refresh_*: обновление выключено.
        assert_eq!(cfg.helpdesk.refresh_path, "");
        assert_eq!(cfg.helpdesk.refresh_method, "POST");
        assert_eq!(cfg.helpdesk.refresh_token_pointer, "/access_token");
    }

    #[test]
    fn validate_clamps_and_rejects() {
        let mut cfg = sample();
        cfg.run.poll_interval_ms = 1;
        cfg.validate().unwrap();
        assert_eq!(cfg.run.poll_interval_ms, MIN_POLL_INTERVAL_MS);

        let mut bad = sample();
        bad.helpdesk.claim_path = "/api/assign".into();
        assert!(bad.validate().is_err());
        let mut bad = sample();
        bad.helpdesk.base_url = "support.example.com".into();
        assert!(bad.validate().is_err());
    }
}
