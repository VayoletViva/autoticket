//! Первый запуск спрашивает данные и сохраняет их; повторный — ничего не спрашивает.

use std::io::Cursor;

use autoticket::setup::ensure_config;

#[tokio::test]
async fn first_run_asks_then_never_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    // url, способ входа, логин, пароль, «настроить пути?», токен бота, chat_id, интервал
    let script = "https://support.example.com\n1\nagent\nsecret-pw\nn\n123:ABC\n111\n\n";
    let mut input = Cursor::new(script.as_bytes());
    let mut out = Vec::new();
    let cfg = ensure_config(&path, true, false, &mut input, &mut out, None).await.unwrap();
    assert!(path.exists());
    assert_eq!(cfg.helpdesk.username, "agent");
    assert_eq!(cfg.helpdesk.password, "secret-pw");
    assert_eq!(cfg.telegram.chat_id, 111);
    assert!(!out.is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // Повторный запуск: ни вопросов, ни чтения ввода.
    let mut empty = Cursor::new(&b""[..]);
    let mut out2 = Vec::new();
    let again = ensure_config(&path, true, false, &mut empty, &mut out2, None).await.unwrap();
    assert!(out2.is_empty(), "{}", String::from_utf8_lossy(&out2));
    assert_eq!(empty.position(), 0);
    assert!(again == cfg);
}

#[tokio::test]
async fn no_config_without_terminal_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let mut input = Cursor::new(&b"whatever\n"[..]);
    let mut out = Vec::new();
    let err = ensure_config(&dir.path().join("config.toml"), false, false, &mut input, &mut out, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("autoticket setup"));
    assert_eq!(input.position(), 0);
}
