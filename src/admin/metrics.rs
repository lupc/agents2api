//! 请求统计：基于 SQLite 事件表的内存聚合 + 窗口查询。
//!
//! 记录维度：时间（本地时区归日）、上游、模型、成功/失败、流式/非流式、
//! prompt/completion tokens、总耗时、TTFB。
//!
//! 与旧实现（JSON 文件 + 内存权威）的区别：
//! - 记录落 **SQLite**（`usage_events`，见 [`super::db`]），重启不丢；
//! - 查询时 `SELECT` 窗口内原始行，再用本模块既有的聚合逻辑汇总，
//!   因此原有分位数 / 成功率口径不变（已被测试覆盖）。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::db::{Db, UsageRow};

/// 一次请求的统计记录（供 [`MetricsStore::record`] 转发到 DB）。
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

#[derive(Default, Clone)]
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

/// 单日统计。
#[derive(Default, Clone)]
pub struct DayStats {
    pub total: Counter,
    pub stream: Counter,
    pub non_stream: Counter,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub upstreams: BTreeMap<String, Counter>,
    pub models: BTreeMap<String, Counter>,
    pub duration_ms: Vec<u64>,
    pub ttfb_ms: Vec<u64>,
    pub model_duration_ms: BTreeMap<String, Vec<u64>>,
    pub model_ttfb_ms: BTreeMap<String, Vec<u64>>,
}

impl DayStats {
    fn record(&mut self, r: &UsageRow) {
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
            self.duration_ms.push(r.duration_ms);
            if r.ttfb_ms > 0 {
                self.ttfb_ms.push(r.ttfb_ms);
            }
            self.model_duration_ms
                .entry(r.model.clone())
                .or_default()
                .push(r.duration_ms);
            if r.ttfb_ms > 0 {
                self.model_ttfb_ms
                    .entry(r.model.clone())
                    .or_default()
                    .push(r.ttfb_ms);
            }
        }
    }
}

fn day_key() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 统计查询器（持 DB 句柄；记录由 [`super::db::Recorder`] 异步写入）。
pub struct MetricsStore {
    db: Db,
}

impl MetricsStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 记录一次请求：换算本地日并转发到 DB 写入队列（非阻塞）。
    pub fn record(&self, r: Record, recorder: &super::db::Recorder) {
        let now = chrono::Local::now();
        recorder.usage(super::db::UsageEvent {
            ts_ms: now.timestamp_millis(),
            day: day_key(),
            upstream: r.upstream,
            model: r.model,
            stream: r.stream,
            ok: r.ok,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            duration_ms: r.duration_ms,
            ttfb_ms: r.ttfb_ms,
        });
    }

    /// 拉取窗口内的按日聚合（内部工具）。
    async fn days(&self, days: usize) -> Result<Vec<(String, DayStats)>, String> {
        let cutoff = (chrono::Local::now() - chrono::Duration::days(days as i64 - 1))
            .format("%Y-%m-%d")
            .to_string();
        let rows = self.db.usage_rows(&cutoff).await?;
        let mut by_day: BTreeMap<String, DayStats> = BTreeMap::new();
        for r in &rows {
            by_day.entry(r.day.clone()).or_default().record(r);
        }
        Ok(by_day.into_iter().collect())
    }

    /// 汇总：近 N 天的合计 + 今日。
    pub async fn overview(&self, days: usize) -> Result<Value, String> {
        let today = day_key();
        let selected = self.days(days).await?;

        let mut agg = Counter::default();
        let mut prompt = 0u64;
        let mut completion = 0u64;
        let mut durations: Vec<u64> = Vec::new();
        let mut today_stats: Option<DayStats> = None;
        for (day, d) in &selected {
            agg.requests += d.total.requests;
            agg.ok += d.total.ok;
            agg.errors += d.total.errors;
            prompt += d.prompt_tokens;
            completion += d.completion_tokens;
            durations.extend_from_slice(&d.duration_ms);
            if *day == today {
                today_stats = Some(d.clone());
            }
        }
        Ok(json!({
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
        }))
    }

    /// 按天用量明细。
    pub async fn usage(&self, days: usize) -> Result<Value, String> {
        let selected = self.days(days).await?;
        let rows: Vec<Value> = selected
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
                    "avg_duration_ms": avg(&d.duration_ms),
                    "p50_duration_ms": percentile(&d.duration_ms, 50.0),
                    "p95_duration_ms": percentile(&d.duration_ms, 95.0),
                    "max_duration_ms": d.duration_ms.iter().copied().max().unwrap_or(0),
                    "avg_ttfb_ms": avg(&d.ttfb_ms),
                    "upstreams": counters_to_json(&d.upstreams),
                    "models": counters_to_json(&d.models),
                })
            })
            .collect();
        Ok(json!({ "window_days": days, "days": rows }))
    }

    /// 模型速率统计（跨所选窗口聚合每模型）。
    pub async fn model_latency(&self, days: usize) -> Result<Value, String> {
        let selected = self.days(days).await?;
        let mut durations: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut ttfbs: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut counters: BTreeMap<String, Counter> = BTreeMap::new();
        for (_, d) in &selected {
            for (m, v) in &d.model_duration_ms {
                durations.entry(m.clone()).or_default().extend_from_slice(v);
            }
            for (m, v) in &d.model_ttfb_ms {
                ttfbs.entry(m.clone()).or_default().extend_from_slice(v);
            }
            for (m, c) in &d.models {
                let e = counters.entry(m.clone()).or_default();
                e.requests += c.requests;
                e.ok += c.ok;
                e.errors += c.errors;
            }
        }
        let mut all_models: std::collections::BTreeSet<String> = durations.keys().cloned().collect();
        all_models.extend(counters.keys().cloned());
        let mut rows: Vec<Value> = all_models
            .iter()
            .map(|model| {
                let dur = durations.get(model).cloned().unwrap_or_default();
                let ttfb = ttfbs.get(model).cloned().unwrap_or_default();
                let c = counters.get(model).cloned().unwrap_or_default();
                json!({
                    "model": model,
                    "samples": dur.len(),
                    "requests": c.requests,
                    "ok": c.ok,
                    "errors": c.errors,
                    "avg_duration_ms": avg(&dur),
                    "p50_duration_ms": percentile(&dur, 50.0),
                    "p95_duration_ms": percentile(&dur, 95.0),
                    "max_duration_ms": dur.iter().copied().max().unwrap_or(0),
                    "avg_ttfb_ms": avg(&ttfb),
                    "p95_ttfb_ms": percentile(&ttfb, 95.0),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            b["samples"]
                .as_u64()
                .unwrap_or(0)
                .cmp(&a["samples"].as_u64().unwrap_or(0))
        });
        Ok(json!({ "window_days": days, "models": rows }))
    }
}

// ---- 工具 ----

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

    fn row(day: &str, upstream: &str, model: &str, ok: bool, dur: u64) -> UsageRow {
        UsageRow {
            day: day.to_string(),
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

    fn agg(rows: &[UsageRow]) -> DayStats {
        let mut d = DayStats::default();
        for r in rows {
            d.record(r);
        }
        d
    }

    #[test]
    fn test_day_aggregation_counters() {
        let d = agg(&[
            row("2026-01-01", "trae", "glm-5.2", true, 100),
            row("2026-01-01", "trae", "glm-5.2", true, 200),
            row("2026-01-01", "buddy", "deepseek", false, 50),
        ]);
        assert_eq!(d.total.requests, 3);
        assert_eq!(d.total.ok, 2);
        assert_eq!(d.total.errors, 1);
        assert_eq!(d.prompt_tokens, 30);
        assert_eq!(d.completion_tokens, 15);
        assert_eq!(rate(d.total.ok, d.total.requests), 66.67);
    }

    #[test]
    fn test_model_percentiles() {
        let rows: Vec<UsageRow> = [10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100]
            .iter()
            .map(|d| row("2026-01-01", "trae", "m1", true, *d))
            .collect();
        let d = agg(&rows);
        let v = &d.model_duration_ms["m1"];
        assert_eq!(percentile(v, 50.0), 50);
        assert_eq!(percentile(v, 95.0), 100);
        assert_eq!(v.iter().copied().max(), Some(100));
    }

    #[test]
    fn test_failed_requests_not_sampled() {
        let d = agg(&[row("2026-01-01", "trae", "m1", false, 999)]);
        assert!(d.model_duration_ms.get("m1").is_none());
        assert_eq!(d.models["m1"].errors, 1);
    }

    #[test]
    fn test_stream_vs_nonstream() {
        let mut r = row("2026-01-01", "trae", "m", true, 10);
        r.stream = true;
        let d = agg(&[r, row("2026-01-01", "trae", "m", true, 10)]);
        assert_eq!(d.stream.requests, 1);
        assert_eq!(d.non_stream.requests, 1);
    }
}
