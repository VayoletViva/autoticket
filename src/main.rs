use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::error;

use autoticket::autostart::{self, EXIT_CONFIG};
use autoticket::config;
use autoticket::helpdesk::Helpdesk;
use autoticket::metrics::{Summary, summarize};
use autoticket::setup::{self, SecretFn};
use autoticket::telegram::TelegramApi;
use autoticket::{logging, worker};

#[derive(Parser)]
#[command(
    name = "autoticket",
    version,
    about = "Автоматически берёт новые тикеты техподдержки и присылает уведомления в Telegram",
    after_help = "Без подкоманды выполняется `run`. Первый запуск: `autoticket setup`."
)]
struct Cli {
    /// Папка с config.toml, логами и claims.csv (по умолчанию системная папка
    /// пользователя или $AUTOTICKET_CONFIG_DIR)
    #[arg(long, global = true, value_name = "DIR")]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Работать: следить за новыми тикетами и брать их (по умолчанию)
    Run {
        /// Без вывода в консоль (для автозапуска; на Windows скрывает окно)
        #[arg(long)]
        background: bool,
    },
    /// Ввести или изменить данные для входа и Telegram
    Setup {
        /// Не проверять онлайн (бот, вход на сайт) перед сохранением
        #[arg(long)]
        no_verify: bool,
    },
    /// Отправить тестовое сообщение в Telegram
    TestNotify,
    /// Замерить задержку запросов по тёплому соединению (тикеты не берутся)
    Bench {
        /// Число запросов
        #[arg(long, default_value_t = 20)]
        n: usize,
        /// Пауза между запросами, мс (не меньше 100)
        #[arg(long, default_value_t = 200)]
        interval_ms: u64,
        /// Замерить произвольный URL без авторизации (вместо списка тикетов)
        #[arg(long)]
        url: Option<String>,
    },
    /// Включить автозапуск при старте системы
    InstallAutostart,
    /// Отключить автозапуск
    UninstallAutostart,
    /// Показать, где лежат конфиг, логи и claims.csv
    Paths,
}

struct Dirs {
    config_dir: PathBuf,
    data_dir: PathBuf,
    /// Папка задана явно (флагом или переменной окружения): её нужно передать автозапуску.
    explicit: Option<PathBuf>,
}

impl Dirs {
    fn new(flag: Option<PathBuf>) -> Self {
        if let Some(d) = flag {
            let d = d.canonicalize().unwrap_or(d);
            return Self { config_dir: d.clone(), data_dir: d.clone(), explicit: Some(d) };
        }
        let explicit = std::env::var_os(config::CONFIG_DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .map(|d| d.canonicalize().unwrap_or(d));
        Self { config_dir: config::config_dir(), data_dir: config::data_dir(), explicit }
    }
    fn config_path(&self) -> PathBuf {
        self.config_dir.join(config::CONFIG_FILE)
    }
    fn session_path(&self) -> PathBuf {
        self.config_dir.join(autoticket::helpdesk::SESSION_FILE)
    }
    fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

fn read_secret(prompt: &str) -> std::io::Result<String> {
    rpassword::prompt_password(prompt)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let dirs = Dirs::new(cli.config_dir);
    let cmd = cli.cmd.unwrap_or(Cmd::Run { background: false });

    match cmd {
        Cmd::Run { background } => {
            #[cfg(windows)]
            if background {
                hide_console();
            }
            let _guard = logging::init(Some(&dirs.log_dir()), !background, None);
            cmd_run(&dirs).await
        }
        other => {
            let _guard = logging::init(None, true, Some("warn"));
            let res = match other {
                Cmd::Setup { no_verify } => cmd_setup(&dirs, !no_verify).await,
                Cmd::TestNotify => cmd_test_notify(&dirs).await,
                Cmd::Bench { n, interval_ms, url } => cmd_bench(&dirs, n, interval_ms, url).await,
                Cmd::InstallAutostart => cmd_install(&dirs),
                Cmd::UninstallAutostart => autostart::uninstall(),
                Cmd::Paths => {
                    println!("config: {}", dirs.config_path().display());
                    println!("сессия: {}", dirs.session_path().display());
                    println!("логи:   {}", dirs.log_dir().display());
                    println!("замеры: {}", dirs.data_dir.join("claims.csv").display());
                    Ok(())
                }
                Cmd::Run { .. } => unreachable!(),
            };
            match res {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Ошибка: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

#[cfg(windows)]
fn hide_console() {
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }
    // SAFETY: FreeConsole не принимает аргументов и лишь отсоединяет процесс от консоли.
    unsafe {
        FreeConsole();
    }
}

async fn cmd_run(dirs: &Dirs) -> ExitCode {
    let path = dirs.config_path();
    let interactive = std::io::stdin().is_terminal();
    let secret: SecretFn = &read_secret;
    let cfg = {
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let mut out = std::io::stdout();
        setup::ensure_config(&path, interactive, true, &mut input, &mut out, interactive.then_some(secret)).await
    };
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    match worker::run(cfg, Some(path), dirs.data_dir.clone(), shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn cmd_setup(dirs: &Dirs, verify: bool) -> Result<()> {
    let path = dirs.config_path();
    let interactive = std::io::stdin().is_terminal();
    let secret: SecretFn = &read_secret;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    println!("Настройка autoticket. Файл: {}\n", path.display());
    setup::configure(&path, verify, &mut input, &mut out, interactive.then_some(secret)).await?;
    println!("Дальше: `autoticket run` (проверить вручную) и `autoticket install-autostart`.");
    if autostart_hint_needed() {
        println!("Если автозапуск уже включён, перезапустите службу, чтобы применить новые настройки.");
    }
    Ok(())
}

fn autostart_hint_needed() -> bool {
    true
}

fn load_existing(dirs: &Dirs) -> Result<config::Config> {
    let path = dirs.config_path();
    let mut cfg = config::load(&path)?
        .with_context(|| format!("{} (ожидался файл {})", setup::NO_CONFIG_MSG, path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

async fn cmd_test_notify(dirs: &Dirs) -> Result<()> {
    let cfg = load_existing(dirs)?;
    let api = TelegramApi::new(&cfg.telegram, Duration::from_secs(10))?;
    api.send_message("🔔 autoticket: тестовое уведомление. Если вы это видите, бот настроен правильно.")
        .await?;
    println!("Сообщение отправлено в чат {}.", cfg.telegram.chat_id);
    Ok(())
}

fn print_summary(label: &str, s: &[Duration]) {
    if let Some(sum) = summarize(s) {
        println!("{}", sum.table_row(label));
    }
}

async fn cmd_bench(dirs: &Dirs, n: usize, interval_ms: u64, url: Option<String>) -> Result<()> {
    let n = n.clamp(1, 200);
    let interval = Duration::from_millis(interval_ms.max(100));
    let mut warm = Vec::with_capacity(n);
    let cold;
    let target;
    if let Some(url) = url {
        autoticket::init_tls();
        let client = reqwest::Client::builder()
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("autoticket/", env!("CARGO_PKG_VERSION"), " bench"))
            .build()?;
        let get = |c: &reqwest::Client| {
            let c = c.clone();
            let url = url.clone();
            async move {
                let t0 = Instant::now();
                let r = c.get(&url).send().await?;
                let el = t0.elapsed();
                let status = r.status();
                let _ = r.bytes().await;
                Ok::<_, reqwest::Error>((el, status))
            }
        };
        let (c0, st) = get(&client).await.context("запрос не удался")?;
        cold = c0;
        target = format!("GET {url} (HTTP {})", st.as_u16());
        for _ in 0..n {
            tokio::time::sleep(interval).await;
            warm.push(get(&client).await?.0);
        }
    } else {
        let cfg = load_existing(dirs)?;
        let hd = Helpdesk::new(cfg.helpdesk.clone(), Duration::from_millis(cfg.run.request_timeout_ms))?
            .with_session_file(dirs.session_path());
        let t0 = Instant::now();
        hd.login().await?;
        cold = t0.elapsed();
        target = format!("{} {}", cfg.helpdesk.list_method, hd.url(&cfg.helpdesk.list_path));
        for _ in 0..n {
            tokio::time::sleep(interval).await;
            warm.push(hd.probe().await?);
        }
    }
    println!("Цель: {target}\n");
    println!("{}", Summary::TABLE_HEADER);
    print_summary("первый запрос (DNS + TCP + TLS + запрос)", &[cold]);
    print_summary("тёплое соединение (keep-alive)", &warm);
    println!(
        "\nЗахват тикета — это такой же запрос по тёплому соединению плюс обработка на сервере,\n\
         поэтому строка «тёплое соединение» — нижняя граница времени захвата."
    );
    Ok(())
}

fn cmd_install(dirs: &Dirs) -> Result<()> {
    let path = dirs.config_path();
    if !Path::new(&path).exists() {
        bail!("сначала выполните `autoticket setup` (нет файла {})", path.display());
    }
    autostart::install(dirs.explicit.as_deref())
}
