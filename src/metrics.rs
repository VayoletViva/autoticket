//! Замеры времени захвата: строка в `claims.csv` и запись в лог.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

pub const CSV_HEADER: &str = "timestamp_utc,ticket_id,claim_rtt_ms,detect_to_claim_ms,outcome";

pub struct Metrics {
    path: Option<PathBuf>,
    file: Mutex<Option<File>>,
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Metrics {
    /// Метрики без файла (только лог).
    pub fn disabled() -> Self {
        Self { path: None, file: Mutex::new(None) }
    }

    pub fn open(path: &Path) -> Self {
        Self { path: Some(path.to_path_buf()), file: Mutex::new(None) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn file<'a>(&self, slot: &'a mut Option<File>) -> Option<&'a mut File> {
        if slot.is_none() {
            let path = self.path.as_ref()?;
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let is_new = std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true);
            match OpenOptions::new().create(true).append(true).open(path) {
                Ok(mut f) => {
                    if is_new {
                        let _ = writeln!(f, "{CSV_HEADER}");
                    }
                    *slot = Some(f);
                }
                Err(e) => {
                    warn!("не удалось открыть {}: {e}", path.display());
                    return None;
                }
            }
        }
        slot.as_mut()
    }

    /// Записать результат захвата. `rtt` = None, если ответа на захват не было.
    pub fn record(&self, ticket_id: &str, rtt: Option<Duration>, detect_to_claim: Duration, outcome: &str) {
        let rtt_s = rtt.map(|d| format!("{:.3}", ms(d))).unwrap_or_default();
        info!(
            ticket = ticket_id,
            outcome,
            claim_rtt_ms = %rtt_s,
            detect_to_claim_ms = %format!("{:.3}", ms(detect_to_claim)),
            "замер захвата"
        );
        let line = format!(
            "{},{},{},{:.3},{}",
            humantime::format_rfc3339_millis(SystemTime::now()),
            csv_field(ticket_id),
            rtt_s,
            ms(detect_to_claim),
            outcome
        );
        let mut slot = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = self.file(&mut slot)
            && let Err(e) = writeln!(f, "{line}")
        {
            warn!("не удалось записать claims.csv: {e}");
            *slot = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub min: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
    pub mean: Duration,
}

/// min / медиана / p95 / max (перцентили методом ближайшего ранга).
pub fn summarize(samples: &[Duration]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort();
    let rank = |p: f64| {
        let r = ((p / 100.0) * v.len() as f64).ceil() as usize;
        v[r.clamp(1, v.len()) - 1]
    };
    let total: Duration = v.iter().sum();
    Some(Summary {
        n: v.len(),
        min: v[0],
        p50: rank(50.0),
        p95: rank(95.0),
        max: v[v.len() - 1],
        mean: total / v.len() as u32,
    })
}

impl Summary {
    pub fn table_row(&self, label: &str) -> String {
        format!(
            "| {label} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
            self.n,
            ms(self.min),
            ms(self.p50),
            ms(self.p95),
            ms(self.max),
            ms(self.mean)
        )
    }

    pub const TABLE_HEADER: &'static str =
        "| замер | n | min, мс | p50, мс | p95, мс | max, мс | среднее, мс |\n|---|---|---|---|---|---|---|";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_percentiles() {
        let v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let s = summarize(&v).unwrap();
        assert_eq!(s.min, Duration::from_millis(1));
        assert_eq!(s.p50, Duration::from_millis(50));
        assert_eq!(s.p95, Duration::from_millis(95));
        assert_eq!(s.max, Duration::from_millis(100));
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn writes_csv_with_header() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claims.csv");
        let m = Metrics::open(&p);
        m.record("42", Some(Duration::from_micros(1500)), Duration::from_micros(2000), "claimed");
        m.record("a,b", None, Duration::from_micros(10), "failed");
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert!(lines[1].ends_with(",42,1.500,2.000,claimed"), "{}", lines[1]);
        assert!(lines[2].contains("\"a,b\",,0.010,failed"), "{}", lines[2]);
    }
}
