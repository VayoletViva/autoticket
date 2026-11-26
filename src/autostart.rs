//! Автозапуск при старте системы: systemd (Linux), Планировщик заданий
//! (Windows), LaunchAgent (macOS). Тексты unit/plist/XML строятся чистыми
//! функциями (покрыты тестами), установка выполняется системными командами.

use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use std::process::Command;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use anyhow::Context;
use anyhow::{Result, bail};

pub const SERVICE_NAME: &str = "autoticket";
pub const LAUNCHD_LABEL: &str = "com.autoticket";
/// Код выхода «нет/ошибка конфигурации» (EX_CONFIG): systemd не перезапускает.
pub const EXIT_CONFIG: u8 = 78;

fn systemd_quote(s: &str) -> String {
    let s = s.replace('%', "%%");
    if s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s
    }
}

/// systemd user unit.
pub fn systemd_unit(exe: &Path, config_dir: Option<&Path>) -> String {
    let mut env = String::from("Environment=RUST_LOG=info\n");
    if let Some(d) = config_dir {
        env.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("AUTOTICKET_CONFIG_DIR={}", d.display()))
        ));
    }
    format!(
        "[Unit]\n\
         Description=autoticket: автоматический захват тикетов техподдержки\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} run\n\
         Restart=always\n\
         RestartSec=10\n\
         RestartPreventExitStatus={EXIT_CONFIG}\n\
         {env}\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(&exe.display().to_string())
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// macOS LaunchAgent.
pub fn launchd_plist(exe: &Path, config_dir: Option<&Path>, log_file: &Path) -> String {
    let env = match config_dir {
        Some(d) => format!(
            "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>AUTOTICKET_CONFIG_DIR</key>\n    <string>{}</string>\n  </dict>\n",
            xml_escape(&d.display().to_string())
        ),
        None => String::new(),
    };
    let log = xml_escape(&log_file.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>run</string>
  </array>
{env}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        xml_escape(&exe.display().to_string())
    )
}

/// Задание Планировщика Windows: при входе пользователя, без лимита времени
/// (у schtasks /SC по умолчанию лимит 72 часа), с перезапуском при сбое.
pub fn windows_task_xml(exe: &Path, user: &str, config_dir: Option<&Path>) -> String {
    let mut args = String::new();
    if let Some(d) = config_dir {
        args.push_str(&format!("--config-dir \"{}\" ", d.display()));
    }
    args.push_str("run --background");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>autoticket: автоматический захват тикетов техподдержки</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>{}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        xml_escape(&exe.display().to_string()),
        xml_escape(&args),
        user = xml_escape(user),
    )
}

/// Путь к исполняемому файлу (канонический).
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    // На Windows canonicalize даёт путь вида \\?\C:\..., Планировщику он не нужен.
    let s = exe.display().to_string();
    Ok(PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s)))
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn run_cmd(args: &[&str]) -> std::result::Result<(), String> {
    let line = args.join(" ");
    match Command::new(args[0]).args(&args[1..]).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "`{line}` завершилась с ошибкой: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("не удалось запустить `{line}`: {e}")),
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn report(failures: Vec<String>, manual: &[String]) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    eprintln!("Не всё получилось сделать автоматически:");
    for f in &failures {
        eprintln!("  - {f}");
    }
    eprintln!("Выполните вручную:");
    for m in manual {
        eprintln!("  {m}");
    }
    bail!("автозапуск настроен не полностью")
}

/// `config_dir_override`: папка конфигурации, если она не стандартная.
pub fn install(config_dir_override: Option<&Path>) -> Result<()> {
    let exe = current_exe()?;
    install_for(&exe, config_dir_override)
}

#[cfg(target_os = "linux")]
fn install_for(exe: &Path, cfgdir: Option<&Path>) -> Result<()> {
    let base = directories::BaseDirs::new().context("не найдена домашняя папка")?;
    let unit_dir = base.config_dir().join("systemd").join("user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join(format!("{SERVICE_NAME}.service"));
    std::fs::write(&unit_path, systemd_unit(exe, cfgdir))
        .with_context(|| format!("не удалось записать {}", unit_path.display()))?;
    println!("Создан {}", unit_path.display());
    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".into());
    let steps: Vec<Vec<String>> = vec![
        vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
        vec!["systemctl".into(), "--user".into(), "enable".into(), "--now".into(), format!("{SERVICE_NAME}.service")],
        // Без linger user-сервисы стартуют только после входа пользователя.
        vec!["loginctl".into(), "enable-linger".into(), user],
    ];
    let mut failures = Vec::new();
    for s in &steps {
        let args: Vec<&str> = s.iter().map(String::as_str).collect();
        if let Err(e) = run_cmd(&args) {
            failures.push(e);
        }
    }
    let manual: Vec<String> = steps.iter().map(|s| s.join(" ")).collect();
    report(failures, &manual)?;
    println!("Автозапуск включён. Проверка: systemctl --user status {SERVICE_NAME}; логи: journalctl --user -u {SERVICE_NAME} -f");
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_for(exe: &Path, cfgdir: Option<&Path>) -> Result<()> {
    let base = directories::BaseDirs::new().context("не найдена домашняя папка")?;
    let dir = base.home_dir().join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{LAUNCHD_LABEL}.plist"));
    let log = base.home_dir().join("Library").join("Logs").join("autoticket.log");
    std::fs::write(&path, launchd_plist(exe, cfgdir, &log))?;
    println!("Создан {}", path.display());
    let p = path.display().to_string();
    let _ = run_cmd(&["launchctl", "unload", &p]);
    let mut failures = Vec::new();
    if let Err(e) = run_cmd(&["launchctl", "load", "-w", &p]) {
        failures.push(e);
    }
    report(failures, &[format!("launchctl load -w {p}")])?;
    println!("Автозапуск включён. Проверка: launchctl list | grep {LAUNCHD_LABEL}");
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_for(exe: &Path, cfgdir: Option<&Path>) -> Result<()> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        (_, Ok(u)) => u,
        _ => bail!("не удалось определить имя пользователя (USERNAME)"),
    };
    let xml = windows_task_xml(exe, &user, cfgdir);
    let path = std::env::temp_dir().join("autoticket-task.xml");
    // schtasks ожидает XML в UTF-16 LE с BOM.
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes)?;
    let p = path.display().to_string();
    let mut failures = Vec::new();
    if let Err(e) = run_cmd(&["schtasks", "/Create", "/TN", SERVICE_NAME, "/XML", &p, "/F"]) {
        failures.push(e);
    } else if let Err(e) = run_cmd(&["schtasks", "/Run", "/TN", SERVICE_NAME]) {
        failures.push(e);
    }
    let _ = std::fs::remove_file(&path);
    report(
        failures,
        &[format!("schtasks /Create /TN {SERVICE_NAME} /TR \"\\\"{}\\\" run --background\" /SC ONLOGON /RL LIMITED /F", exe.display())],
    )?;
    println!("Автозапуск включён: задание «{SERVICE_NAME}» в Планировщике заданий (при входе в систему).");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn install_for(_exe: &Path, _cfgdir: Option<&Path>) -> Result<()> {
    bail!("автозапуск для этой ОС не поддерживается: настройте запуск `autoticket run` вручную")
}

pub fn uninstall() -> Result<()> {
    uninstall_impl()
}

#[cfg(target_os = "linux")]
fn uninstall_impl() -> Result<()> {
    let base = directories::BaseDirs::new().context("не найдена домашняя папка")?;
    let unit_path = base.config_dir().join("systemd").join("user").join(format!("{SERVICE_NAME}.service"));
    let mut failures = Vec::new();
    if let Err(e) = run_cmd(&["systemctl", "--user", "disable", "--now", &format!("{SERVICE_NAME}.service")]) {
        failures.push(e);
    }
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)?;
        println!("Удалён {}", unit_path.display());
    }
    let _ = run_cmd(&["systemctl", "--user", "daemon-reload"]);
    report(failures, &[format!("systemctl --user disable --now {SERVICE_NAME}.service")])?;
    println!("Автозапуск отключён.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_impl() -> Result<()> {
    let base = directories::BaseDirs::new().context("не найдена домашняя папка")?;
    let path = base.home_dir().join("Library").join("LaunchAgents").join(format!("{LAUNCHD_LABEL}.plist"));
    let p = path.display().to_string();
    let _ = run_cmd(&["launchctl", "unload", "-w", &p]);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("Автозапуск отключён.");
    Ok(())
}

#[cfg(target_os = "windows")]
fn uninstall_impl() -> Result<()> {
    let _ = run_cmd(&["schtasks", "/End", "/TN", SERVICE_NAME]);
    let mut failures = Vec::new();
    if let Err(e) = run_cmd(&["schtasks", "/Delete", "/TN", SERVICE_NAME, "/F"]) {
        failures.push(e);
    }
    report(failures, &[format!("schtasks /Delete /TN {SERVICE_NAME} /F")])?;
    println!("Автозапуск отключён.");
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn uninstall_impl() -> Result<()> {
    bail!("автозапуск для этой ОС не поддерживается")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_has_exec_and_restart() {
        let u = systemd_unit(Path::new("/opt/autoticket/autoticket"), None);
        assert!(u.contains("ExecStart=/opt/autoticket/autoticket run\n"), "{u}");
        assert!(u.contains("Restart=always"));
        assert!(u.contains("After=network-online.target"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("RestartPreventExitStatus=78"));
        assert!(!u.contains("AUTOTICKET_CONFIG_DIR"));
    }

    #[test]
    fn unit_quotes_paths_and_env() {
        let u = systemd_unit(Path::new("/home/me/my apps/autoticket"), Some(Path::new("/srv/at cfg")));
        assert!(u.contains("ExecStart=\"/home/me/my apps/autoticket\" run"), "{u}");
        assert!(u.contains("Environment=\"AUTOTICKET_CONFIG_DIR=/srv/at cfg\""), "{u}");
    }

    #[test]
    fn plist_and_task_xml() {
        let p = launchd_plist(Path::new("/usr/local/bin/autoticket"), None, Path::new("/tmp/a.log"));
        assert!(p.contains("<string>/usr/local/bin/autoticket</string>"));
        assert!(p.contains("<key>RunAtLoad</key>"));
        let x = windows_task_xml(Path::new(r"C:\Tools\autoticket.exe"), r"PC\Ivan", None);
        assert!(x.contains(r"<Command>C:\Tools\autoticket.exe</Command>"));
        assert!(x.contains("<Arguments>run --background</Arguments>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains(r"<UserId>PC\Ivan</UserId>"));
    }
}
