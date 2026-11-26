//! Цикл опроса и захвата. Ни одна ошибка не завершает процесс: всё
//! логируется, а следующая попытка делается с паузой (backoff).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::config::{self, Config};
use crate::helpdesk::{ClaimOutcome, Helpdesk, HelpdeskError, Ticket};
use crate::metrics::Metrics;
use crate::telegram::{Notifier, RetryPolicy, TelegramApi, format_claim};

/// Сколько раз пробовать захватить тикет, если сервер отвечает ошибкой.
pub const MAX_CLAIM_ATTEMPTS: u32 = 3;
/// Сколько помнить обработанный тикет после того, как он пропал из списка.
pub const SEEN_TTL: Duration = Duration::from_secs(6 * 3600);
/// Через сколько непрерывных ошибок связи слать алерт в Telegram.
pub const NETWORK_ALERT_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub struct State {
    /// Обработанные тикеты (взяты нами или уже заняты) и когда видели последний раз.
    seen: HashMap<String, Instant>,
    /// Неудачные попытки захвата.
    failures: HashMap<String, u32>,
}

impl State {
    pub fn is_seen(&self, id: &str) -> bool {
        self.seen.contains_key(id)
    }
    pub fn seen_len(&self) -> usize {
        self.seen.len()
    }
}

/// Один проход: получить список, захватить все новые тикеты параллельно,
/// отправить уведомления. Возвращает число взятых тикетов.
pub async fn poll_once(
    state: &mut State,
    hd: &Arc<Helpdesk>,
    notifier: &Notifier,
    metrics: &Metrics,
) -> Result<usize, HelpdeskError> {
    let tickets = hd.fetch_new().await?;
    let detected = Instant::now();

    // Ограниченный рост памяти: забываем тикеты, которых давно нет в списке.
    for t in &tickets {
        if let Some(seen_at) = state.seen.get_mut(&t.id) {
            *seen_at = detected;
        }
    }
    state.seen.retain(|_, at| detected.duration_since(*at) < SEEN_TTL);
    state.failures.retain(|id, _| tickets.iter().any(|t| &t.id == id));

    let fresh: Vec<Ticket> = tickets
        .into_iter()
        .filter(|t| {
            !state.seen.contains_key(&t.id)
                && state.failures.get(&t.id).copied().unwrap_or(0) < MAX_CLAIM_ATTEMPTS
        })
        .collect();
    if fresh.is_empty() {
        return Ok(0);
    }

    let mut set = JoinSet::new();
    for t in fresh {
        let hd = Arc::clone(hd);
        set.spawn(async move {
            let res = hd.claim(&t).await;
            (t, res, detected.elapsed())
        });
    }

    let mut claimed = 0;
    let mut fatal: Option<HelpdeskError> = None;
    while let Some(joined) = set.join_next().await {
        let (t, res, d) = match joined {
            Ok(v) => v,
            Err(e) => {
                error!("задача захвата аварийно завершилась: {e}");
                continue;
            }
        };
        match res {
            Ok(ClaimOutcome::Claimed { rtt }) => {
                info!(ticket = %t.id, title = %t.title, "тикет взят за {:.1} мс", rtt.as_secs_f64() * 1e3);
                metrics.record(&t.id, Some(rtt), d, "claimed");
                notifier.send(format_claim(&t, rtt, d, &hd.cfg().ticket_url));
                state.failures.remove(&t.id);
                state.seen.insert(t.id, Instant::now());
                claimed += 1;
            }
            Ok(ClaimOutcome::AlreadyTaken { status }) => {
                info!(ticket = %t.id, "тикет уже занят (HTTP {status})");
                metrics.record(&t.id, None, d, "already_taken");
                state.failures.remove(&t.id);
                state.seen.insert(t.id, Instant::now());
            }
            Ok(ClaimOutcome::Failed(msg)) => {
                register_failure(state, &t, &msg);
                metrics.record(&t.id, None, d, "failed");
            }
            Err(e) if e.is_auth() || matches!(e, HelpdeskError::RateLimited(_)) => {
                metrics.record(&t.id, None, d, "failed");
                fatal = Some(e);
            }
            Err(e) => {
                register_failure(state, &t, &e.to_string());
                metrics.record(&t.id, None, d, "failed");
            }
        }
    }
    match fatal {
        Some(e) => Err(e),
        None => Ok(claimed),
    }
}

fn register_failure(state: &mut State, t: &Ticket, msg: &str) {
    let n = state.failures.entry(t.id.clone()).or_insert(0);
    *n += 1;
    if *n >= MAX_CLAIM_ATTEMPTS {
        error!(ticket = %t.id, "не удалось взять тикет: {msg}. Попыток: {n}, больше не пытаюсь");
    } else {
        warn!(ticket = %t.id, "не удалось взять тикет: {msg} (попытка {n}/{MAX_CLAIM_ATTEMPTS})");
    }
}

struct LoopCtx {
    cfg: Config,
    config_path: Option<PathBuf>,
    notifier: Notifier,
    metrics: Arc<Metrics>,
}

/// Главный цикл с супервизором: если цикл опроса паникует, паника логируется
/// и цикл перезапускается. `shutdown` завершает работу корректно.
pub async fn run(
    cfg: Config,
    config_path: Option<PathBuf>,
    data_dir: PathBuf,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let timeout = Duration::from_millis(cfg.run.request_timeout_ms);
    let api = TelegramApi::new(&cfg.telegram, timeout)?;
    let (notifier, tg_task) = Notifier::spawn(api, RetryPolicy::default());
    let metrics = Arc::new(Metrics::open(&data_dir.join("claims.csv")));
    // Проверяем конфигурацию до старта цикла (ошибка здесь = ошибка настройки).
    Helpdesk::new(cfg.helpdesk.clone(), timeout)?;
    info!(
        "autoticket запущен: {} опрашивается каждые {} мс, замеры пишутся в {}",
        cfg.helpdesk.base_url,
        cfg.run.poll_interval_ms,
        data_dir.join("claims.csv").display()
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let mut shutdown = pin!(shutdown);
    loop {
        let ctx = LoopCtx {
            cfg: cfg.clone(),
            config_path: config_path.clone(),
            notifier: notifier.clone(),
            metrics: Arc::clone(&metrics),
        };
        let mut task = tokio::spawn(poll_loop(ctx, stop_rx.clone()));
        tokio::select! {
            _ = &mut shutdown => {
                info!("получен сигнал остановки, завершаю работу");
                let _ = stop_tx.send(true);
                let _ = tokio::time::timeout(Duration::from_secs(10), &mut task).await;
                break;
            }
            res = &mut task => match res {
                Ok(()) => break,
                Err(e) => {
                    error!("цикл опроса аварийно завершился: {e}; перезапуск через 5 с");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
    drop(notifier);
    // Даём очереди уведомлений дослать сообщения.
    let _ = tokio::time::timeout(Duration::from_secs(10), tg_task).await;
    Ok(())
}

async fn poll_loop(mut ctx: LoopCtx, mut stop: watch::Receiver<bool>) {
    let timeout = Duration::from_millis(ctx.cfg.run.request_timeout_ms);
    let poll = Duration::from_millis(ctx.cfg.run.poll_interval_ms.max(config::MIN_POLL_INTERVAL_MS));
    let max_backoff = Duration::from_millis(ctx.cfg.run.max_backoff_ms).max(poll);
    let mut hd = match Helpdesk::new(ctx.cfg.helpdesk.clone(), timeout) {
        Ok(h) => Arc::new(with_session(h, ctx.config_path.as_deref())),
        Err(e) => {
            error!("{e}");
            return;
        }
    };
    let mut state = State::default();
    let mut delay = Duration::ZERO;
    let mut backoff = poll;
    let mut down_since: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        if *stop.borrow() {
            return;
        }
        let started = Instant::now();
        match poll_once(&mut state, &hd, &ctx.notifier, &ctx.metrics).await {
            Ok(_) => {
                if down_since.take().is_some() {
                    info!("связь с техподдержкой восстановлена");
                }
                backoff = poll;
                delay = poll.saturating_sub(started.elapsed());
            }
            Err(HelpdeskError::RateLimited(d)) => {
                warn!("сервер вернул 429 Too Many Requests, пауза {d:?}");
                delay = d.max(poll);
            }
            Err(e) if e.is_auth() => {
                error!("{e}. Следующая попытка входа через {max_backoff:?}");
                ctx.notifier.alert(
                    "auth",
                    &format!(
                        "{e}. Проверьте логин/пароль или вставьте новый токен: autoticket setup. \
                         Программа продолжает пытаться."
                    ),
                );
                // Если пользователь обновил config.toml (например, новый токен), подхватываем его.
                if let Some(new_hd) = reload_helpdesk(&mut ctx, timeout) {
                    hd = Arc::new(new_hd);
                }
                delay = max_backoff;
            }
            Err(e) => {
                backoff = (backoff * 2).min(max_backoff);
                delay = backoff;
                warn!("{e}; повтор через {delay:?}");
                let since = *down_since.get_or_insert(started);
                if since.elapsed() >= NETWORK_ALERT_AFTER {
                    ctx.notifier.alert(
                        "network",
                        &format!(
                            "техподдержка недоступна уже {} мин: {e}",
                            since.elapsed().as_secs() / 60
                        ),
                    );
                }
            }
        }
    }
}

/// Подключает session.json из папки config.toml (если путь конфига известен).
pub fn with_session(hd: Helpdesk, config_path: Option<&std::path::Path>) -> Helpdesk {
    match config_path.and_then(|p| p.parent()) {
        Some(dir) => hd.with_session_file(dir.join(crate::helpdesk::SESSION_FILE)),
        None => hd,
    }
}

fn reload_helpdesk(ctx: &mut LoopCtx, timeout: Duration) -> Option<Helpdesk> {
    let path = ctx.config_path.as_ref()?;
    let mut cfg = match config::load(path) {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(e) => {
            warn!("не удалось перечитать конфиг: {e:#}");
            return None;
        }
    };
    if cfg.helpdesk == ctx.cfg.helpdesk || cfg.validate().is_err() {
        return None;
    }
    info!("config.toml изменился: применяю новые настройки входа");
    let hd = with_session(Helpdesk::new(cfg.helpdesk.clone(), timeout).ok()?, Some(path));
    ctx.cfg.helpdesk = cfg.helpdesk;
    Some(hd)
}
