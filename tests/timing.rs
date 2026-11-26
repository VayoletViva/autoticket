//! Замер времени захвата на локальном mock-сервере (накладные расходы самой программы).
//! cargo test --release --test timing -- --ignored --nocapture

use std::time::Duration;

use autoticket::config::HelpdeskCfg;
use autoticket::helpdesk::{ClaimOutcome, Helpdesk, Ticket};
use autoticket::metrics::{Summary, summarize};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn claim_latency_on_local_mock() {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": "T"})))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/tickets/\d+/assign$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&s)
        .await;
    let cfg = HelpdeskCfg { base_url: s.uri(), username: "a".into(), password: "p".into(), ..Default::default() };
    let hd = Helpdesk::new(cfg, Duration::from_secs(5)).unwrap();
    hd.login().await.unwrap();
    let mut samples = Vec::new();
    for i in 0..200 {
        let t = Ticket { id: i.to_string(), title: String::new(), extra: vec![], raw: Value::Null };
        match hd.claim(&t).await.unwrap() {
            ClaimOutcome::Claimed { rtt } => samples.push(rtt),
            o => panic!("{o:?}"),
        }
    }
    let sum = summarize(&samples).unwrap();
    println!("{}\n{}", Summary::TABLE_HEADER, sum.table_row("захват, локальный mock, тёплое соединение"));
    assert!(sum.p50 < Duration::from_millis(10));
}
