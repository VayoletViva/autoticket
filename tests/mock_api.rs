//! Тесты с имитацией API техподдержки и Telegram (wiremock).

use std::sync::Arc;
use std::time::Duration;

use autoticket::config::{Config, HelpdeskCfg, TelegramCfg};
use autoticket::helpdesk::{ClaimOutcome, Helpdesk, HelpdeskError, Ticket};
use autoticket::metrics::Metrics;
use autoticket::telegram::{Notifier, RetryPolicy, TelegramApi};
use autoticket::worker::{State, poll_once};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BOT: &str = "123456:TEST-BOT-TOKEN";

fn hd_cfg(base: &str) -> HelpdeskCfg {
    HelpdeskCfg {
        base_url: base.into(),
        username: "agent".into(),
        password: "pw".into(),
        list_path: "/api/tickets".into(),
        ..Default::default()
    }
}

fn tg_cfg(base: &str) -> TelegramCfg {
    TelegramCfg { bot_token: BOT.into(), chat_id: 111, api_base: base.into() }
}

async fn mount_login(s: &MockServer, token: &str) {
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .and(body_partial_json(json!({"login": "agent", "password": "pw"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": token})))
        .mount(s)
        .await;
}

fn fast() -> RetryPolicy {
    RetryPolicy { base: Duration::from_millis(50), max: Duration::from_millis(200), attempts: 5 }
}

async fn wait_requests(s: &MockServer, p: &str, n: usize) -> Vec<Value> {
    for _ in 0..100 {
        let reqs: Vec<Value> = s
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with(p))
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect();
        if reqs.len() >= n {
            return reqs;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("не дождались {n} запросов к {p}");
}

#[tokio::test]
async fn new_ticket_is_claimed_and_notified_once() {
    let hd_srv = MockServer::start().await;
    let tg_srv = MockServer::start().await;
    mount_login(&hd_srv, "T1").await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(header("Authorization", "Bearer T1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 42, "subject": "Не работает принтер", "priority": "high"}
        ])))
        .mount(&hd_srv)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/tickets/42/assign"))
        .and(header("Authorization", "Bearer T1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&hd_srv)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {}})))
        .mount(&tg_srv)
        .await;

    let hd = Arc::new(Helpdesk::new(hd_cfg(&hd_srv.uri()), Duration::from_secs(5)).unwrap());
    let api = TelegramApi::new(&tg_cfg(&tg_srv.uri()), Duration::from_secs(5)).unwrap();
    let (notifier, _h) = Notifier::spawn(api, fast());
    let metrics = Metrics::disabled();
    let mut state = State::default();

    assert_eq!(poll_once(&mut state, &hd, &notifier, &metrics).await.unwrap(), 1);
    assert!(state.is_seen("42"));
    let msgs = wait_requests(&tg_srv, "/sendMessage", 1).await;
    let text = msgs[0]["text"].as_str().unwrap();
    assert!(text.contains("42") && text.contains("Не работает принтер"), "{text}");
    assert_eq!(msgs[0]["chat_id"], 111);

    // Второй проход: тикет всё ещё в списке, но повторно не берётся.
    assert_eq!(poll_once(&mut state, &hd, &notifier, &metrics).await.unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(wait_requests(&tg_srv, "/sendMessage", 1).await.len(), 1);
    hd_srv.verify().await;
}

#[tokio::test]
async fn nested_list_pointer_is_parsed() {
    let s = MockServer::start().await;
    mount_login(&s, "T").await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"items": [
            {"id": "A-1", "subject": "x"}, {"id": 2, "subject": "y"}
        ]}})))
        .mount(&s)
        .await;
    let mut cfg = hd_cfg(&s.uri());
    cfg.list_items_pointer = "/data/items".into();
    let hd = Helpdesk::new(cfg, Duration::from_secs(5)).unwrap();
    let t = hd.fetch_new().await.unwrap();
    assert_eq!(t.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["A-1", "2"]);
}

#[tokio::test]
async fn relogin_once_on_401() {
    let s = MockServer::start().await;
    // Первый вход выдаёт старый токен, второй — новый.
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "OLD"})))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    mount_login(&s, "NEW").await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(header("Authorization", "Bearer OLD"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .and(header("Authorization", "Bearer NEW"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&s)
        .await;
    let hd = Helpdesk::new(hd_cfg(&s.uri()), Duration::from_secs(5)).unwrap();
    assert!(hd.fetch_new().await.unwrap().is_empty());
    s.verify().await;
}

#[tokio::test]
async fn persistent_401_is_auth_expired_not_loop() {
    let s = MockServer::start().await;
    mount_login(&s, "T").await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&s)
        .await;
    let hd = Helpdesk::new(hd_cfg(&s.uri()), Duration::from_secs(5)).unwrap();
    assert!(matches!(hd.fetch_new().await, Err(HelpdeskError::AuthExpired(401))));
    s.verify().await;
}

#[tokio::test]
async fn conflict_means_already_taken() {
    let s = MockServer::start().await;
    mount_login(&s, "T").await;
    Mock::given(method("POST"))
        .and(path("/api/tickets/7/assign"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&s)
        .await;
    let hd = Helpdesk::new(hd_cfg(&s.uri()), Duration::from_secs(5)).unwrap();
    let t = Ticket { id: "7".into(), title: "t".into(), extra: vec![], raw: Value::Null };
    assert_eq!(hd.claim(&t).await.unwrap(), ClaimOutcome::AlreadyTaken { status: 409 });
}

#[tokio::test]
async fn telegram_retries_after_500() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT}/sendMessage")))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {}})))
        .expect(1)
        .mount(&s)
        .await;
    let api = TelegramApi::new(&tg_cfg(&s.uri()), Duration::from_secs(5)).unwrap();
    let (n, h) = Notifier::spawn(api, fast());
    assert!(n.send("✅ Взят тикет #42".into()));
    drop(n);
    tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap();
    s.verify().await;
}

#[tokio::test]
async fn telegram_errors_do_not_leak_token() {
    // Порт 1 закрыт: ошибка соединения.
    let api = TelegramApi::new(&tg_cfg("http://127.0.0.1:1"), Duration::from_secs(2)).unwrap();
    let err = api.send_message("x").await.unwrap_err().to_string();
    assert!(!err.contains("TEST-BOT-TOKEN"), "{err}");
}

#[tokio::test]
async fn run_survives_server_errors_and_claims() {
    let hd_srv = MockServer::start().await;
    let tg_srv = MockServer::start().await;
    mount_login(&hd_srv, "T").await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&hd_srv)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tickets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 5, "subject": "VPN"}])))
        .mount(&hd_srv)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/tickets/5/assign"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&hd_srv)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bot{BOT}/sendMessage")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {}})))
        .mount(&tg_srv)
        .await;
    let mut cfg = Config { helpdesk: hd_cfg(&hd_srv.uri()), telegram: tg_cfg(&tg_srv.uri()), ..Default::default() };
    cfg.run.poll_interval_ms = 250;
    cfg.run.max_backoff_ms = 500;
    cfg.validate().unwrap();
    let dir = tempfile::tempdir().unwrap();
    autoticket::worker::run(cfg, None, dir.path().to_path_buf(), tokio::time::sleep(Duration::from_millis(2000)))
        .await
        .unwrap();
    hd_srv.verify().await;
    let msgs = wait_requests(&tg_srv, "/sendMessage", 1).await;
    assert!(msgs[0]["text"].as_str().unwrap().contains("VPN"));
    let csv = std::fs::read_to_string(dir.path().join("claims.csv")).unwrap();
    assert!(csv.lines().nth(1).unwrap().contains(",5,"), "{csv}");
}
