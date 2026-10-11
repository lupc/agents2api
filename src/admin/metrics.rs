//! 请求统计：内存聚合 + 周期落盘（JSON）。
//!
//! 记录维度：时间（本地时区归日）、上游、模型、成功/失败、流式/非流式、
//! prompt/completion tokens、总耗时、TTFB。
//!
//! 设计取舍（对齐参考实现 TraeWorkAssistant 的 `usage.rs` 语义）：
//! - 内存为权威态，落盘带脏标记 + 节流，进程强杀可能丢最后几秒。
//! - 按天聚合，保留 90 天。
//! - 延迟用滚动采样（天级 cap 512 / 模型级 cap 256），算分位数用最近邻上取整。
//! - 持久化为 JSON 文件而非 SQLite：数据量小（每天每上游一行）、零重依赖。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 天级延迟采样上限。
const LATENCY_CAP: usize = 512;
/// 模型级延迟采样上限。
const MODEL_LATENCY_CAP: usize = 256;
/// 保留天数。
const RETENTION_DAYS: i64 = 90;
/// 落盘最小间隔。
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// 一次请求的统计记录（供 [`MetricsStore::record`]）。
pub struct Record {
    pub upstream: String,
    pub model: String,
    pub stream: bool,
    pub ok: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u64,
    pub ttfb_ms: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Counter {
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
}

impl Counter {
    fn add(&mut self, ok: bool) {
        self.requests += 1;
        if ok {
            self.ok += 1;
        } else {
            self.errors += 1;
        }
    }
}

/// 滚动延迟采样。
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct LatencyAgg {
    pub total_ms: Vec<u64>,
    pub ttfb_ms: Vec<u64>,
}

impl LatencyAgg {
    fn push(&mut self, duration_ms: u64, ttfb_ms: u64, cap: usize) {
        self.total_ms.push(duration_ms);
        if ttfb_ms > 0 {
            self.ttfb_ms.push(ttfb_ms);
        }
        // 超限丢弃最旧的一半，保持滚动窗口
        if self.total_ms.len() > cap {
            let drop = self.total_ms.len() / 2;
            self.total_ms.drain(..drop);
            if self.ttfb_ms.len() > cap {
                self.ttfb_ms.drain(..self.ttfb_ms.len() / 2);
            }
        }
    }
}

/// 单日统计。
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct DayStats {
    pub total: Counter,
    pub stream: Counter,
    pub non_stream: Counter,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub upstreams: BTreeMap<String, Counter>,
    pub models: BTreeMap<String, Counter>,
    pub latency: LatencyAgg,
    pub model_latency: BTreeMap<String, LatencyAgg>,
}

impl DayStats {
    fn record(&mut self, r: &Record) {
        self.total.add(r.ok);
        if r.stream {
            self.stream.add(r.ok);
        } else {
            self.non_stream.add(r.ok);
        }
        self.prompt_tokens += r.prompt_tokens;
        self.completion_tokens += r.completion_tokens;
        self.upstreams.entry(r.upstream.clone()).or_default().add(r.ok);
        self.models.entry(r.model.clone()).or_default().add(r.ok);
        if r.ok {
            self.latency.push(r.duration_ms, r.ttfb_ms, LATENCY_CAP);
            self.model_latency
                .entry(r.model.clone())
                .or_default()
                .push(r.duration_ms, r.ttfb_ms, MODEL_LATENCY_CAP);
        }
    }
}

/// 落盘文件结构。
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct UsageFile {
    pub days: BTreeMap<String, DayStats>,
}

impl UsageFile {
    fn prune(&mut self) {
        let cutoff = (chrono::Local::now() - chrono::Duration::days(RETENTION_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        let old: Vec<String> = self
            .days
            .keys()
            .filter(|d| d.as_str() < cutoff.as_str())
            .cloned()
            .collect();
        for d in old {
            self.days.remove(&d);
        }
    }
}

fn day_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 统计存储器。
pub struct MetricsStore {
    inner: Mutex<UsageFile>,
    dirty: AtomicBool,
    path: PathBuf,
    last_flush: Mutex<Option<Instant>>,
}

impl MetricsStore {
    /// 从 `<data_dir>/stats.json` 载入（不存在则空）。
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join("stats.json");
        let mut file: UsageFile = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        file.prune();
        Self {
            inner: Mutex::new(file),
            dirty: AtomicBool::new(false),
            path,
            last_flush: Mutex::new(None),
        }
    }

    /// 记录一次请求。
    pub fn record(&self, r: Record) {
        let mut file = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let day = day_key();
        file.days.entry(day).or_default().record(&r);
        // 跨天时顺便裁剪
        file.prune();
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// 若距上次落盘超过 [`FLUSH_INTERVAL`]，且存在脏数据，则写盘。
    pub fn flush_if_due(&self) {
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        {
            let last = self.last_flush.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = *last {
                if t.elapsed() < FLUSH_INTERVAL {
                    return;
                }
            }
        }
        self.flush();
    }

    /// 立即落盘（忽略节流）。供后台「立即保存」与退出时调用。
    pub fn flush(&self) {
        let text = {
            let file = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match serde_json::to_string(&*file) {
                Ok(t) => t,
                Err(_) => return,
            }
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // 原子写：临时文件 + rename
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, &text).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
            self.dirty.store(false, Ordering::Relaxed);
            *self.last_flush.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    // ---- 查询 ----

    /// 汇总：近 N 天的合计 + 今日。
    pub fn overview(&self, days: usize) -> Value {
        let file = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let today = day_key();
        let selected = select_days(&file, days);

        let mut agg = Counter::default();
        let mut prompt = 0u64;
        let mut completion = 0u64;
        let mut durations: Vec<u64> = Vec::new();
        for d in &selected {
            agg.requests += d.total.requests;
            agg.ok += d.total.ok;
            agg.errors += d.total.errors;
            prompt += d.prompt_tokens;
            completion += d.completion_tokens;
            durations.extend_from_slice(&d.latency.total_ms);
        }
        let today_stats = file.days.get(&today);
        json!({
            "window_days": days,
            "requests": agg.requests,
            "ok": agg.ok,
            "errors": agg.errors,
            "success_rate": rate(agg.ok, agg.requests),
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
            "avg_duration_ms": avg(&durations),
            "p95_duration_ms": percentile(&durations, 95.0),
            "today": today_stats.map(|d| json!({
                "requests": d.total.requests,
                "ok": d.total.ok,
                "errors": d.total.errors,
                "prompt_tokens": d.prompt_tokens,
                "completion_tokens": d.completion_tokens,
            })),
        })
    }

    /// 按天用量明细。
    pub fn usage(&self, days: usize) -> Value {
        let file = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let selected: Vec<Value> = select_days_with_key(&file, days)
            .into_iter()
            .map(|(day, d)| {
                json!({
                    "date": day,
                    "requests": d.total.requests,
                    "ok": d.total.ok,
                    "errors": d.total.errors,
                    "success_rate": rate(d.total.ok, d.total.requests),
                    "stream_requests": d.stream.requests,
                    "non_stream_requests": d.non_stream.requests,
                    "prompt_tokens": d.prompt_tokens,
                    "completion_tokens": d.completion_tokens,
                    "total_tokens": d.prompt_tokens + d.completion_tokens,
                    "avg_duration_ms": avg(&d.latency.total_ms),
                    "p50_duration_ms": percentile(&d.latency.total_ms, 50.0),
                    "p95_duration_ms": percentile(&d.latency.total_ms, 95.0),
                    "max_duration_ms": d.latency.total_ms.iter().copied().max().unwrap_or(0),
                    "avg_ttfb_ms": avg(&d.latency.ttfb_ms),
                    "upstreams": counters_to_json(&d.upstreams),
                    "models": counters_to_json(&d.models),
                })
            })
            .collect();
        json!({ "window_days": days, "days": selected })
    }

    /// 模型速率统计（跨所选窗口聚合每模型）。
    pub fn model_latency(&self, days: usize) -> Value {
        let file = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let selected = select_days(&file, days);
        let mut merged: BTreeMap<String, LatencyAgg> = BTreeMap::new();
        let mut counters: BTreeMap<String, Counter> = BTreeMap::new();
        for d in &selected {
            for (m, agg) in &d.model_latency {
                let e = merged.entry(m.clone()).or_default();
                e.total_ms.extend_from_slice(&agg.total_ms);
                e.ttfb_ms.extend_from_slice(&agg.ttfb_ms);
            }
            for (m, c) in &d.models {
                let e = counters.entry(m.clone()).or_default();
                e.requests += c.requests;
                e.ok += c.ok;
                e.errors += c.errors;
            }
        }
        // 以「出现过请求的模型」为全集：成功过的有延迟样本，只失败过的 samples=0 也列出
        let mut all_models: std::collections::BTreeSet<String> = merged.keys().cloned().collect();
        all_models.extend(counters.keys().cloned());
        let mut rows: Vec<Value> = all_models
            .iter()
            .map(|model| {
                let agg = merged.get(model).cloned().unwrap_or_default();
                let c = counters.get(model).cloned().unwrap_or_default();
                json!({
                    "model": model,
                    "samples": agg.total_ms.len(),
                    "requests": c.requests,
                    "ok": c.ok,
                    "errors": c.errors,
                    "avg_duration_ms": avg(&agg.total_ms),
                    "p50_duration_ms": percentile(&agg.total_ms, 50.0),
                    "p95_duration_ms": percentile(&agg.total_ms, 95.0),
                    "max_duration_ms": agg.total_ms.iter().copied().max().unwrap_or(0),
                    "avg_ttfb_ms": avg(&agg.ttfb_ms),
                    "p95_ttfb_ms": percentile(&agg.ttfb_ms, 95.0),
                })
            })
            .collect();
        // 按吞吐（样本数）降序
        rows.sort_by(|a, b| {
            b["samples"]
                .as_u64()
                .unwrap_or(0)
                .cmp(&a["samples"].as_u64().unwrap_or(0))
        });
        json!({ "window_days": days, "models": rows })
    }
}

// ---- 工具 ----

fn select_days(file: &UsageFile, days: usize) -> Vec<DayStats> {
    select_days_with_key(file, days)
        .into_iter()
        .map(|(_, d)| d)
        .collect()
}

fn select_days_with_key(file: &UsageFile, days: usize) -> Vec<(String, DayStats)> {
    let cutoff = (chrono::Local::now() - chrono::Duration::days(days as i64 - 1))
        .format("%Y-%m-%d")
        .to_string();
    file.days
        .iter()
        .filter(|(k, _)| k.as_str() >= cutoff.as_str())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn counters_to_json(m: &BTreeMap<String, Counter>) -> Value {
    let mut rows: Vec<Value> = m
        .iter()
        .map(|(k, c)| {
            json!({
                "name": k,
                "requests": c.requests,
                "ok": c.ok,
                "errors": c.errors,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b["requests"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["requests"].as_u64().unwrap_or(0))
    });
    Value::Array(rows)
}

fn rate(ok: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (ok as f64 / total as f64 * 10000.0).round() / 100.0
    }
}

fn avg(v: &[u64]) -> u64 {
    if v.is_empty() {
        0
    } else {
        v.iter().sum::<u64>() / v.len() as u64
    }
}

/// 最近邻上取整分位数。
fn percentile(v: &[u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(s.len() - 1);
    s[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> MetricsStore {
        let dir = std::env::temp_dir().join(format!("c2a-metrics-{tag}-{}", crate::rand_hex(4)));
        let _ = std::fs::create_dir_all(&dir);
        MetricsStore::load(&dir)
    }

    fn rec(upstream: &str, model: &str, ok: bool, dur: u64) -> Record {
        Record {
            upstream: upstream.to_string(),
            model: model.to_string(),
            stream: false,
            ok,
            prompt_tokens: 10,
            completion_tokens: 5,
            duration_ms: dur,
            ttfb_ms: dur / 2,
        }
    }

    #[test]
    fn test_record_and_overview() {
        let s = store("ov");
        s.record(rec("trae", "glm-5.2", true, 100));
        s.record(rec("trae", "glm-5.2", true, 200));
        s.record(rec("codebuddy", "deepseek-v4-flash", false, 50));
        let ov = s.overview(7);
        assert_eq!(ov["requests"], 3);
        assert_eq!(ov["ok"], 2);
        assert_eq!(ov["errors"], 1);
        assert_eq!(ov["prompt_tokens"], 30);
        assert_eq!(ov["completion_tokens"], 15);
        assert_eq!(ov["success_rate"], 66.67);
    }

    #[test]
    fn test_model_latency_percentiles() {
        let s = store("ml");
        for d in [10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            s.record(rec("trae", "m1", true, d));
        }
        let ml = s.model_latency(7);
        let row = &ml["models"][0];
        assert_eq!(row["model"], "m1");
        assert_eq!(row["samples"], 10);
        assert_eq!(row["max_duration_ms"], 100);
        // p50：ceil(0.5*10)=5 → 第 5 个（升序） = 50
        assert_eq!(row["p50_duration_ms"], 50);
        // p95：ceil(0.95*10)=10 → 第 10 个 = 100
        assert_eq!(row["p95_duration_ms"], 100);
    }

    #[test]
    fn test_failed_requests_not_sampled_for_latency() {
        let s = store("fail");
        s.record(rec("trae", "m1", false, 999));
        let ml = s.model_latency(7);
        assert_eq!(ml["models"][0]["samples"], 0);
        assert_eq!(ml["models"][0]["errors"], 1);
    }

    #[test]
    fn test_latency_rolling_cap() {
        let s = store("cap");
        for i in 0..(LATENCY_CAP as u64 + 100) {
            s.record(rec("trae", "m1", true, i));
        }
        let file = s.inner.lock().unwrap();
        let day = &file.days[&day_key()];
        assert!(day.latency.total_ms.len() <= LATENCY_CAP);
    }

    #[test]
    fn test_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("c2a-metrics-persist-{}", crate::rand_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let s = MetricsStore::load(&dir);
            s.record(rec("trae", "m1", true, 100));
            s.flush();
        }
        let s2 = MetricsStore::load(&dir);
        let ov = s2.overview(7);
        assert_eq!(ov["requests"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_retention_prunes_old_days() {
        let dir = std::env::temp_dir().join(format!("c2a-metrics-ret-{}", crate::rand_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        // 写入一个 200 天前的日期
        let old = (chrono::Local::now() - chrono::Duration::days(200))
            .format("%Y-%m-%d")
            .to_string();
        let mut f = UsageFile::default();
        f.days.insert(old.clone(), DayStats::default());
        f.days.insert(day_key(), DayStats::default());
        std::fs::write(dir.join("stats.json"), serde_json::to_string(&f).unwrap()).unwrap();
        let s = MetricsStore::load(&dir);
        let file = s.inner.lock().unwrap();
        assert!(!file.days.contains_key(&old));
        assert!(file.days.contains_key(&day_key()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stream_vs_nonstream_counters() {
        let s = store("svn");
        let mut r = rec("trae", "m", true, 10);
        r.stream = true;
        s.record(r);
        s.record(rec("trae", "m", true, 10));
        let u = s.usage(7);
        assert_eq!(u["days"][0]["stream_requests"], 1);
        assert_eq!(u["days"][0]["non_stream_requests"], 1);
    }
}
