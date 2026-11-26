//! Первый запуск: интерактивные вопросы, онлайн-проверка и сохранение конфига.
//!
//! Ввод и вывод передаются параметрами, поэтому настройку можно прогнать в
//! тестах со скриптованным вводом.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config::{self, AuthMode, Config};
use crate::helpdesk::Helpdesk;
use crate::telegram::TelegramApi;

/// Функция чтения секрета без эха (в main: rpassword на терминале).
pub type SecretFn<'a> = &'a dyn Fn(&str) -> std::io::Result<String>;

pub const NO_CONFIG_MSG: &str =
    "нет конфигурации: запустите `autoticket setup` в терминале, чтобы ввести данные для входа";

struct Io<'a, R, W> {
    input: &'a mut R,
    out: &'a mut W,
    secret: Option<SecretFn<'a>>,
}

impl<R: BufRead, W: Write> Io<'_, R, W> {
    fn line(&mut self) -> Result<Option<String>> {
        let mut s = String::new();
        if self.input.read_line(&mut s)? == 0 {
            return Ok(None);
        }
        Ok(Some(s.trim().to_string()))
    }

    fn prompt(&mut self, q: &str, default: &str) -> Result<()> {
        if default.is_empty() {
            write!(self.out, "{q}: ")?;
        } else {
            write!(self.out, "{q} [{default}]: ")?;
        }
        self.out.flush()?;
        Ok(())
    }

    /// Вопрос со значением по умолчанию. Enter — принять, «-» — пустое значение.
    fn ask(&mut self, q: &str, default: &str) -> Result<String> {
        self.prompt(q, default)?;
        Ok(match self.line()? {
            None => {
                writeln!(self.out)?;
                default.to_string()
            }
            Some(s) if s.is_empty() => default.to_string(),
            Some(s) if s == "-" => String::new(),
            Some(s) => s,
        })
    }

    fn ask_required(&mut self, q: &str, default: &str, check: impl Fn(&str) -> Result<()>) -> Result<String> {
        for _ in 0..5 {
            self.prompt(q, default)?;
            let v = match self.line()? {
                None => bail!("ввод закончился, а «{q}» обязателен"),
                Some(s) if s.is_empty() => default.to_string(),
                Some(s) => s,
            };
            if v.is_empty() {
                writeln!(self.out, "  Это поле обязательно.")?;
                continue;
            }
            match check(&v) {
                Ok(()) => return Ok(v),
                Err(e) => writeln!(self.out, "  {e}")?,
            }
        }
        bail!("слишком много неверных ответов на вопрос «{q}»")
    }

    /// Секрет. Если есть прежнее значение, Enter оставляет его.
    fn ask_secret(&mut self, q: &str, existing: &str) -> Result<String> {
        let q = if existing.is_empty() {
            q.to_string()
        } else {
            format!("{q} (Enter — оставить прежний)")
        };
        for _ in 0..5 {
            let v = match self.secret {
                Some(f) => {
                    self.out.flush()?;
                    f(&format!("{q}: ")).context("не удалось прочитать ввод")?.trim().to_string()
                }
                None => {
                    self.prompt(&q, "")?;
                    match self.line()? {
                        Some(s) => s,
                        None if !existing.is_empty() => String::new(),
                        None => bail!("ввод закончился, а «{q}» обязателен"),
                    }
                }
            };
            if !v.is_empty() {
                return Ok(v);
            }
            if !existing.is_empty() {
                return Ok(existing.to_string());
            }
            writeln!(self.out, "  Это поле обязательно.")?;
        }
        bail!("не введено значение «{q}»")
    }

    fn yes(&mut self, q: &str, default: bool) -> Result<bool> {
        let d = if default { "Y/n" } else { "y/N" };
        write!(self.out, "{q} [{d}]: ")?;
        self.out.flush()?;
        Ok(match self.line()?.as_deref().map(str::to_lowercase) {
            Some(s) if ["y", "yes", "д", "да"].contains(&s.as_str()) => true,
            Some(s) if ["n", "no", "н", "нет"].contains(&s.as_str()) => false,
            _ => default,
        })
    }
}

fn check_url(s: &str) -> Result<()> {
    let u = url::Url::parse(s).map_err(|e| anyhow::anyhow!("не похоже на URL ({e}); пример: https://support.example.com"))?;
    if !matches!(u.scheme(), "http" | "https") {
        bail!("адрес должен начинаться с https:// или http://");
    }
    Ok(())
}

/// Задаёт вопросы и возвращает новый конфиг (ещё не сохранённый).
/// `can_autodetect_chat`: можно ли оставить chat_id пустым (его определит онлайн-проверка).
pub fn run_setup<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    secret: Option<SecretFn<'_>>,
    existing: Option<&Config>,
    can_autodetect_chat: bool,
) -> Result<Config> {
    let mut io = Io { input, out, secret };
    let mut cfg = existing.cloned().unwrap_or_default();

    writeln!(io.out, "== Сайт техподдержки ==")?;
    let base = cfg.helpdesk.base_url.clone();
    cfg.helpdesk.base_url = io
        .ask_required("Адрес сайта техподдержки", &base, check_url)?
        .trim_end_matches('/')
        .to_string();

    let mode_default = if cfg.helpdesk.auth_mode == AuthMode::Token { "2" } else { "1" };
    let mode = io.ask("Способ входа: 1 — логин и пароль, 2 — готовый токен/cookie из браузера (вход через Telegram, SSO, капча)", mode_default)?;
    cfg.helpdesk.auth_mode = if mode.trim() == "2" { AuthMode::Token } else { AuthMode::Password };

    match cfg.helpdesk.auth_mode {
        AuthMode::Password => {
            let user = cfg.helpdesk.username.clone();
            cfg.helpdesk.username = io.ask_required("Логин", &user, |_| Ok(()))?;
            let old = cfg.helpdesk.password.clone();
            cfg.helpdesk.password = io.ask_secret("Пароль (ввод не отображается)", &old)?;
        }
        AuthMode::Token => {
            let old = cfg.helpdesk.token.clone();
            cfg.helpdesk.token = io.ask_secret("Токен или cookie из DevTools (без слова Bearer)", &old)?;
            let h = cfg.helpdesk.auth_header.clone();
            cfg.helpdesk.auth_header = io.ask("Имя заголовка (Authorization или Cookie)", &h)?;
            let def_scheme = if cfg.helpdesk.auth_header.eq_ignore_ascii_case("cookie") {
                String::new()
            } else {
                cfg.helpdesk.auth_scheme.clone()
            };
            cfg.helpdesk.auth_scheme = io.ask("Схема перед токеном («-» — без схемы)", &def_scheme)?;
        }
    }

    if io.yes("Настроить пути API сейчас? (иначе — значения по умолчанию, их можно поправить в config.toml)", false)? {
        let h = &mut cfg.helpdesk;
        if h.auth_mode == AuthMode::Password {
            h.login_path = io.ask("Путь входа (POST)", &h.login_path.clone())?;
            h.login_username_field = io.ask("Поле логина в JSON", &h.login_username_field.clone())?;
            h.login_password_field = io.ask("Поле пароля в JSON", &h.login_password_field.clone())?;
            h.token_pointer =
                io.ask("JSON Pointer токена в ответе («-» — сессия через cookie)", &h.token_pointer.clone())?;
        }
        h.list_path = io.ask("Путь списка новых тикетов (GET)", &h.list_path.clone())?;
        h.list_items_pointer =
            io.ask("JSON Pointer массива тикетов («-» — корень ответа)", &h.list_items_pointer.clone())?;
        h.id_pointer = io.ask("JSON Pointer id тикета", &h.id_pointer.clone())?;
        h.title_pointer = io.ask("JSON Pointer темы тикета", &h.title_pointer.clone())?;
        h.claim_method = io.ask("HTTP-метод захвата", &h.claim_method.clone())?.to_uppercase();
        loop {
            h.claim_path = io.ask("Путь захвата ({id} — номер тикета)", &h.claim_path.clone())?;
            if h.claim_path.contains("{id}") {
                break;
            }
            writeln!(io.out, "  Путь должен содержать {{id}}.")?;
        }
        h.claim_body = io.ask("JSON-тело захвата («-» — без тела)", &h.claim_body.clone())?;
        h.ticket_url = io.ask("Ссылка на тикет для сообщения («-» — не нужна)", &h.ticket_url.clone())?;
        if h.auth_mode == AuthMode::Token {
            h.refresh_path = io.ask("Путь обновления сессии (refresh), если есть («-» — нет)", &h.refresh_path.clone())?;
            if !h.refresh_path.is_empty() {
                h.refresh_token_pointer = io.ask(
                    "JSON Pointer нового токена в ответе refresh («-» — сайт обновляет только cookie)",
                    &h.refresh_token_pointer.clone(),
                )?;
            }
        }
    } else {
        writeln!(
            io.out,
            "  Используются пути по умолчанию ({} и {}). Если у вашего сайта другие, \
             отредактируйте config.toml (см. README, раздел «Как найти эндпоинты»).",
            cfg.helpdesk.list_path, cfg.helpdesk.claim_path
        )?;
    }

    writeln!(io.out, "== Telegram ==")?;
    let old = cfg.telegram.bot_token.clone();
    cfg.telegram.bot_token = io.ask_secret("Токен бота от @BotFather", &old)?;
    let chat_default = if cfg.telegram.chat_id != 0 { cfg.telegram.chat_id.to_string() } else { String::new() };
    let chat_q = if can_autodetect_chat {
        "chat_id (число; Enter — определить автоматически)"
    } else {
        "chat_id (число)"
    };
    for _ in 0..5 {
        let v = io.ask(chat_q, &chat_default)?;
        if v.is_empty() && can_autodetect_chat {
            cfg.telegram.chat_id = 0;
            break;
        }
        match v.parse::<i64>() {
            Ok(id) if id != 0 => {
                cfg.telegram.chat_id = id;
                break;
            }
            _ => writeln!(io.out, "  Нужно число, например 123456789 (узнать: @userinfobot).")?,
        }
    }
    if cfg.telegram.chat_id == 0 && !can_autodetect_chat {
        bail!("chat_id не задан");
    }

    let poll = cfg.run.poll_interval_ms.to_string();
    let poll = io.ask(
        &format!("Интервал опроса, мс (не меньше {})", config::MIN_POLL_INTERVAL_MS),
        &poll,
    )?;
    cfg.run.poll_interval_ms = poll.parse().unwrap_or(cfg.run.poll_interval_ms).max(config::MIN_POLL_INTERVAL_MS);
    Ok(cfg)
}

/// Онлайн-проверка: бот, chat_id (автоопределение), тестовое сообщение,
/// пробный вход и запрос списка (без захвата). Возвращает `false`, если
/// пользователь решил не сохранять.
pub async fn verify_and_complete<R: BufRead, W: Write>(
    cfg: &mut Config,
    input: &mut R,
    out: &mut W,
) -> Result<bool> {
    let mut io = Io { input, out, secret: None };
    let mut problems = Vec::new();
    let timeout = Duration::from_millis(cfg.run.request_timeout_ms.max(5000));

    writeln!(io.out, "\nПроверяю Telegram-бота…")?;
    let mut api = TelegramApi::new(&cfg.telegram, timeout)?;
    match api.get_me().await {
        Ok(name) => {
            writeln!(io.out, "  Бот @{name} найден.")?;
            if cfg.telegram.chat_id == 0 {
                writeln!(io.out, "  Откройте @{name} в Telegram и отправьте ему /start (жду до 3 минут)…")?;
                io.out.flush()?;
                match detect_chat_id(&api, Duration::from_secs(180)).await {
                    Some((id, who)) => {
                        writeln!(io.out, "  chat_id = {id} ({who})")?;
                        cfg.telegram.chat_id = id;
                    }
                    None => {
                        writeln!(io.out, "  Сообщение не пришло.")?;
                        let v = io.ask_required("Введите chat_id вручную", "", |s| {
                            s.parse::<i64>().map(|_| ()).map_err(|_| anyhow::anyhow!("нужно число"))
                        })?;
                        cfg.telegram.chat_id = v.parse()?;
                    }
                }
                api.chat_id = cfg.telegram.chat_id;
            }
            match api.send_message("autoticket подключён ✅ Сюда будут приходить взятые тикеты.").await {
                Ok(()) => writeln!(io.out, "  Тестовое сообщение отправлено.")?,
                Err(e) => {
                    writeln!(io.out, "  Не удалось отправить сообщение: {e}")?;
                    problems.push("Telegram: сообщение не отправлено");
                }
            }
        }
        Err(e) => {
            writeln!(io.out, "  {e}")?;
            problems.push("Telegram: бот не отвечает или неверный токен");
            if cfg.telegram.chat_id == 0 {
                let v = io.ask_required("Введите chat_id вручную", "", |s| {
                    s.parse::<i64>().map(|_| ()).map_err(|_| anyhow::anyhow!("нужно число"))
                })?;
                cfg.telegram.chat_id = v.parse()?;
            }
        }
    }

    writeln!(io.out, "Проверяю вход в техподдержку и список тикетов (без захвата)…")?;
    match Helpdesk::new(cfg.helpdesk.clone(), timeout) {
        Ok(hd) => match hd.fetch_new().await {
            Ok(list) => writeln!(io.out, "  Вход выполнен, сейчас в списке тикетов: {}.", list.len())?,
            Err(e) => {
                writeln!(io.out, "  {e}")?;
                problems.push("техподдержка: проверка не прошла");
            }
        },
        Err(e) => {
            writeln!(io.out, "  {e}")?;
            problems.push("техподдержка: ошибка настроек");
        }
    }

    if problems.is_empty() {
        return Ok(true);
    }
    writeln!(io.out, "\nЕсть проблемы: {}.", problems.join("; "))?;
    io.yes("Сохранить настройки всё равно?", false)
}

async fn detect_chat_id(api: &TelegramApi, wait: Duration) -> Option<(i64, String)> {
    let deadline = Instant::now() + wait;
    let mut offset = 0i64;
    while Instant::now() < deadline {
        match api.get_updates(offset, 25).await {
            Ok(updates) => {
                for u in updates {
                    if let Some(id) = u.get("update_id").and_then(Value::as_i64) {
                        offset = offset.max(id + 1);
                    }
                    if let Some(chat) = u.pointer("/message/chat")
                        && let Some(id) = chat.get("id").and_then(Value::as_i64)
                    {
                        let who = chat
                            .get("username")
                            .and_then(Value::as_str)
                            .map(|u| format!("@{u}"))
                            .or_else(|| chat.get("title").and_then(Value::as_str).map(str::to_string))
                            .or_else(|| chat.get("first_name").and_then(Value::as_str).map(str::to_string))
                            .unwrap_or_default();
                        // Подтверждаем получение, чтобы не читать это сообщение повторно.
                        let _ = api.get_updates(offset, 0).await;
                        return Some((id, who));
                    }
                }
            }
            Err(e) => {
                eprintln!("  getUpdates: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    None
}

/// Полная настройка: вопросы, (опционально) онлайн-проверка, сохранение.
pub async fn configure<R: BufRead, W: Write>(
    path: &Path,
    verify: bool,
    input: &mut R,
    out: &mut W,
    secret: Option<SecretFn<'_>>,
) -> Result<Config> {
    let existing = config::load(path).ok().flatten();
    let mut cfg = run_setup(input, out, secret, existing.as_ref(), verify)?;
    if verify && !verify_and_complete(&mut cfg, input, out).await? {
        bail!("настройка отменена, ничего не сохранено");
    }
    cfg.validate()?;
    config::save(path, &cfg)?;
    writeln!(out, "\nНастройки сохранены в {} (доступ только для вашего пользователя).", path.display())?;
    Ok(cfg)
}

/// Конфиг для запуска: если уже сохранён — без вопросов. Иначе (первый запуск)
/// — настройка, но только при наличии терминала: при автозапуске без
/// конфигурации возвращается ошибка, ничего не ожидая на stdin.
pub async fn ensure_config<R: BufRead, W: Write>(
    path: &Path,
    interactive: bool,
    verify: bool,
    input: &mut R,
    out: &mut W,
    secret: Option<SecretFn<'_>>,
) -> Result<Config> {
    if let Some(mut cfg) = config::load(path)? {
        cfg.validate()
            .with_context(|| format!("ошибка в {}; исправьте файл или запустите `autoticket setup`", path.display()))?;
        return Ok(cfg);
    }
    if !interactive {
        bail!("{NO_CONFIG_MSG} (ожидался файл {})", path.display());
    }
    writeln!(out, "Первый запуск autoticket: ответьте на несколько вопросов.\n")?;
    configure(path, verify, input, out, secret).await
}
