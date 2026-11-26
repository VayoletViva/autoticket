//! Режим «готовый токен/cookie» (вход через Telegram, SSO): cookie-сессия
//! следует за Set-Cookie, переживает перезапуск через session.json, а короткий
//! Bearer обновляется через refresh-эндпоинт. Всё на имитации API (wiremock).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use autoticket::config::{AuthMode, HelpdeskCfg, TelegramCfg};
use autoticket::helpdesk::{Helpdesk, HelpdeskError, SESSION_FILE, fingerprint};
use autoticket::metrics::Metrics;
use autoticket::telegram::{Notifier, RetryPolicy, TelegramApi};
use autoticket::worker::{State, poll_once};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BOT: &str = "123456:TEST-BOT-TOKEN";

fn cookie_cfg(base: &str, token: &str) -> HelpdeskCfg {
    HelpdeskCfg {
        base_url: base.into(),
        auth_mode: AuthMode::Token,
        token: token.into(),
        auth_header: "Cookie".into(),
        auth_scheme: String::new(),
        list_path: "/api/tickets".into(),
        ..Default::default()
    }
}

fn bearer_cfg(base: &str) -> HelpdeskCfg {
    HelpdeskCfg {
        base_url: base.into(),
        auth_mode: AuthMode::Token,
        token: "old".into(),
        list_path: "/api/tickets".into(),
        refresh_path: "/api/auth/refresh".into(),
        ..Default::default()
    }
}

/// Matcher: в заголовке Cookie есть все `must` и нет ни одного из `must_not`.
fn cookie_has(must: &'static [&'static str], must_not: &'static [&'static str]) -> impl Fn(&Request) -> bool {
    move |req: &Request| {
        let all: Vec<String> = req
            .headers
            .get_all("cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        let joined = all.join("; ");
        let pairs: Vec<&str> = joined.split(';').map(str::trim).collect();
        must.iter().all(|m| pairs.contains(m)) && !must_not.iter().any(|m| pairs.contains(m))
    }
}

fn session_path(dir: &Path) -> std::path::PathBuf {
    dir.join(SESSION_FILE)
}

fn hd(cfg: HelpdeskCfg, dir: &Path) -> Helpdesk {
    Helpdesk::new(cfg, Duration::from_secs(5)).unwrap().with_session_file(session_path(dir))
}

/// Первый опрос со старым cookie получает Set-Cookie с новым, второй опрос
/// должен прийти уже с новым (и с остальными вставленными cookie).
async fn set_cookie_flow(s: &MockServer, dir: &Path) {
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(cookie_has(&["sid=old", "theme=dark"], &[]))
        .respond_with(ResponseTemplate::new(200).insert_header("Set-Cookie", "sid=new; Path=/").set_body_json(json!([])))
        .expect(1)
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(cookie_has(&["sid=new", "theme=dark"], &["sid=old"]))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(s)
        .await;
    let h = hd(cookie_cfg(&s.uri(), "sid=old; theme=dark"), dir);
    h.fetch_new().await.expect("первый опрос");
    h.fetch_new().await.expect("второй опрос с обновлённым cookie");
}

#[tokio::test]
async fn cookie_mode_follows_set_cookie() {
    let s = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    set_cookie_flow(&s, dir.path()).await;
    // Второй запрос пришёл с sid=new: проверяем, что второй mock сработал.
    let reqs = s.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(cookie_has(&["sid=new", "theme=dark"], &["sid=old"])(&reqs[1]));
    s.verify().await;

    let text = std::fs::read_to_string(session_path(dir.path())).expect("session.json создан");
    let v: Value = serde_json::from_str(&text).unwrap();
    let cookie = v["cookie"].as_str().unwrap();
    assert!(cookie.contains("sid=new") && cookie.contains("theme=dark") && !cookie.contains("sid=old"), "{cookie}");
    assert_eq!(v["source"].as_u64().unwrap(), fingerprint("sid=old; theme=dark"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(session_path(dir.path())).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn session_file_survives_restart() {
    let s = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    set_cookie_flow(&s, dir.path()).await;
    s.reset().await;

    // «Перезапуск»: новый клиент с тем же config.toml и тем же session.json.
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(cookie_has(&["sid=new", "theme=dark"], &["sid=old"]))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 1, "subject": "x"}])))
        .expect(1)
        .mount(&s)
        .await;
    let h = hd(cookie_cfg(&s.uri(), "sid=old; theme=dark"), dir.path());
    assert_eq!(h.fetch_new().await.unwrap().len(), 1);
    s.verify().await;
}

#[tokio::test]
async fn new_pasted_cookie_overrides_session_file() {
    let s = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    // session.json остался от прошлого вставленного cookie.
    std::fs::write(
        session_path(dir.path()),
        json!({"source": fingerprint("sid=old"), "cookie": "sid=stale"}).to_string(),
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(cookie_has(&["sid=fresh"], &["sid=stale"]))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&s)
        .await;
    // Пользователь вставил новый cookie, ещё и с префиксом «Cookie:».
    let h = hd(cookie_cfg(&s.uri(), "Cookie: sid=fresh"), dir.path());
    h.fetch_new().await.unwrap();
    s.verify().await;
    let text = std::fs::read_to_string(session_path(dir.path())).unwrap();
    assert!(text.contains("sid=fresh") && !text.contains("stale"), "{text}");
}

#[tokio::test]
async fn bearer_refresh_on_401() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(header("Authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "new"})))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(header("Authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 5, "subject": "y"}])))
        .expect(2)
        .mount(&s)
        .await;
    let h = Helpdesk::new(bearer_cfg(&s.uri()), Duration::from_secs(5)).unwrap();
    let t = h.fetch_new().await.unwrap();
    assert_eq!(t[0].id, "5");
    // Следующий опрос сразу с новым токеном, без повторного refresh.
    h.fetch_new().await.unwrap();
    s.verify().await;
}

#[tokio::test]
async fn refresh_failure_is_auth_expired() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/auth/refresh"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    let h = Helpdesk::new(bearer_cfg(&s.uri()), Duration::from_secs(5)).unwrap();
    let err = h.fetch_new().await.unwrap_err();
    assert!(matches!(err, HelpdeskError::AuthExpired(401)), "{err:?}");
    s.verify().await;
}

#[tokio::test]
async fn token_without_refresh_is_auth_expired_without_extra_requests() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    let mut cfg = bearer_cfg(&s.uri());
    cfg.refresh_path = String::new();
    let h = Helpdesk::new(cfg, Duration::from_secs(5)).unwrap();
    assert!(matches!(h.fetch_new().await.unwrap_err(), HelpdeskError::AuthExpired(401)));
    s.verify().await;
}

#[tokio::test]
async fn cookie_mode_claim_and_notify() {
    let hd_srv = MockServer::start().await;
    let tg_srv = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(cookie_has(&["sid=tg-session"], &[]))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 42, "subject": "Не работает принтер"}
        ])))
        .mount(&hd_srv)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/tickets/42/assign"))
        .and(cookie_has(&["sid=tg-session"], &[]))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&hd_srv)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {}})))
        .mount(&tg_srv)
        .await;

    let h = Arc::new(hd(cookie_cfg(&hd_srv.uri(), "sid=tg-session"), dir.path()));
    let tg = TelegramCfg { bot_token: BOT.into(), chat_id: 111, api_base: tg_srv.uri() };
    let api = TelegramApi::new(&tg, Duration::from_secs(5)).unwrap();
    let policy = RetryPolicy { base: Duration::from_millis(50), max: Duration::from_millis(200), attempts: 5 };
    let (notifier, _task) = Notifier::spawn(api, policy);
    let mut state = State::default();
    assert_eq!(poll_once(&mut state, &h, &notifier, &Metrics::disabled()).await.unwrap(), 1);
    assert_eq!(poll_once(&mut state, &h, &notifier, &Metrics::disabled()).await.unwrap(), 0);

    let mut text = String::new();
    for _ in 0..100 {
        let reqs = tg_srv.received_requests().await.unwrap();
        if let Some(r) = reqs.first() {
            let v: Value = serde_json::from_slice(&r.body).unwrap();
            text = v["text"].as_str().unwrap_or_default().to_string();
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(text.contains("42") && text.contains("Не работает принтер"), "{text}");
    hd_srv.verify().await;
}
