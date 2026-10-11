//! 逐轮对话的吞吐统计：预填充速度（prefill TPS）与解码速度（decode TPS）。
//!
//! 两个指标的定义（均按「生成阶段」切分，与业界口径一致）：
//! - **预填充速度** `prefill_tps = prompt_tokens / ttft_seconds`
//!   —— 从发出请求到首个 token 返回的耗时（TTFT）内处理了多少输入 token。
//!   TTFT 覆盖网络往返 + 上游 prefill，因此反映「吃 prompt」的速度。
//! - **解码速度** `decode_tps = completion_tokens / (duration - ttft)_seconds`
//!   —— 首个 token 之后，平均每秒吐出多少 token，反映生成流畅度。
//!
//! 两者按「每轮等权」求平均（先累加比值再除以轮数），避免大请求拉高整体均值。
//!
//! 存储：SQLite `speed_turns` 表（见 [`super::db`]）——逐轮原始记录落库，
//! 查询时 `SELECT` 窗口内行、在内存中按小时 / (上游,模型) 聚合。
//! 相比旧的 JSON 文件方案，重启不丢数据，且可按任意窗口重算。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::db::{Db, SpeedRow};

/// 生成阶段的最小可信窗口（毫秒）。
///
/// 低于此值不计算解码速度：上游常把整段响应缓冲后一次性下发，此时
/// `duration - ttft` 只剩几毫秒，`completion_tokens / 几毫秒` 会算出
/// 十万级以上的假速度（例如窗口 2ms、532 tokens → 266000 tps）。
/// 这类样本不代表真实生成能力，记 0（不计入均值）比记假值更有意义。
const MIN_DECODE_WINDOW_MS: u64 = 100;

/// 一轮对话的原始计量（进入 [`SpeedStore::record`] 的输入）。
pub struct Turn {
    pub upstream: String,
    pub model: String,
    pub stream: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 输入中命中上游缓存的 tokens（用于区分真实预填充与缓存命中）。
    pub cached_tokens: u64,
    /// 首 token 耗时（含上游往返 + prefill）。
    pub ttft_ms: u64,
    /// 整轮耗时。
    pub duration_ms: u64,
}

/// 一轮对话的速度明细（聚合中间态）。
#[derive(Debug, Clone, Default)]
pub struct TurnSample {
    pub upstream: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub prefill_tps: f64,
    pub prefill_fresh_tps: f64,
    pub decode_tps: f64,
}

/// 单个模型在一个小时桶内的累加器。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelSpeedAgg {
    pub turns: u64,
    pub prefill_sum: f64,
    pub prefill_n: u64,
    pub prefill_fresh_sum: f64,
    pub prefill_fresh_n: u64,
    pub decode_sum: f64,
    pub decode_n: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

impl ModelSpeedAgg {
    fn add(&mut self, s: &TurnSample) {
        self.turns += 1;
        if s.prefill_tps > 0.0 {
            self.prefill_sum += s.prefill_tps;
            self.prefill_n += 1;
        }
        if s.prefill_fresh_tps > 0.0 {
            self.prefill_fresh_sum += s.prefill_fresh_tps;
            self.prefill_fresh_n += 1;
        }
        if s.decode_tps > 0.0 {
            self.decode_sum += s.decode_tps;
            self.decode_n += 1;
        }
        self.prompt_tokens += s.prompt_tokens;
        self.completion_tokens += s.completion_tokens;
        self.cached_tokens += s.cached_tokens;
    }

    /// 按「每轮等权」求平均（分母为有效样本数）。
    fn avg_prefill(&self) -> f64 {
        if self.prefill_n == 0 {
            0.0
        } else {
            round2(self.prefill_sum / self.prefill_n as f64)
        }
    }

    /// 扣除缓存命中后的预填充速度均值。
    fn avg_prefill_fresh(&self) -> f64 {
        if self.prefill_fresh_n == 0 {
            0.0
        } else {
            round2(self.prefill_fresh_sum / self.prefill_fresh_n as f64)
        }
    }

    fn avg_decode(&self) -> f64 {
        if self.decode_n == 0 {
            0.0
        } else {
            round2(self.decode_sum / self.decode_n as f64)
        }
    }

    /// 合并另一个累加器（跨时间桶汇总时用）。
    fn merge(&mut self, o: &ModelSpeedAgg) {
        self.turns += o.turns;
        self.prefill_sum += o.prefill_sum;
        self.prefill_n += o.prefill_n;
        self.prefill_fresh_sum += o.prefill_fresh_sum;
        self.prefill_fresh_n += o.prefill_fresh_n;
        self.decode_sum += o.decode_sum;
        self.decode_n += o.decode_n;
        self.prompt_tokens += o.prompt_tokens;
        self.completion_tokens += o.completion_tokens;
        self.cached_tokens += o.cached_tokens;
    }
}

/// 小时桶速度聚合。
#[derive(Debug, Clone, Default)]
pub struct HourSpeedAgg {
    /// 按 `(upstream, model)` 聚合；同名模型可能被多个上游暴露。
    pub models: BTreeMap<(String, String), ModelSpeedAgg>,
}

impl HourSpeedAgg {
    fn add(&mut self, s: &TurnSample) {
        self.models
            .entry((s.upstream.clone(), s.model.clone()))
            .or_default()
            .add(s);
    }

    /// 汇总为单个累加器（跨上游/模型）。
    fn total(&self) -> ModelSpeedAgg {
        let mut t = ModelSpeedAgg::default();
        for m in self.models.values() {
            t.merge(m);
        }
        t
    }
}

/// 四舍五入到两位，避免浮点噪声写进 JSON。
fn round2(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    (v * 100.0).round() / 100.0
}

/// 由原始计量算出派生速度（预填充 / 未命中预填充 / 解码）。
fn derive(t: &Turn) -> SpeedRow {
    let now = chrono::Local::now();
    let prefill_tps = if t.ttft_ms > 0 && t.prompt_tokens > 0 {
        round2(t.prompt_tokens as f64 / (t.ttft_ms as f64 / 1000.0))
    } else {
        0.0
    };
    let fresh = t.prompt_tokens.saturating_sub(t.cached_tokens);
    let prefill_fresh_tps = if t.ttft_ms > 0 && fresh > 0 {
        round2(fresh as f64 / (t.ttft_ms as f64 / 1000.0))
    } else {
        0.0
    };
    let gen_ms = t.duration_ms.saturating_sub(t.ttft_ms);
    let decode_tps = if gen_ms >= MIN_DECODE_WINDOW_MS && t.completion_tokens > 0 {
        round2(t.completion_tokens as f64 / (gen_ms as f64 / 1000.0))
    } else {
        0.0
    };
    SpeedRow {
        time: now.format("%H:%M:%S").to_string(),
        hour: now.format("%Y-%m-%d %H").to_string(),
        upstream: t.upstream.clone(),
        model: t.model.clone(),
        stream: t.stream,
        prompt_tokens: t.prompt_tokens,
        cached_tokens: t.cached_tokens,
        completion_tokens: t.completion_tokens,
        ttft_ms: t.ttft_ms,
        duration_ms: t.duration_ms,
        gen_ms,
        prefill_tps,
        prefill_fresh_tps,
        decode_tps,
    }
}

/// 速度统计查询器（持 DB 句柄；记录由 [`super::db::Recorder`] 异步写入）。
pub struct SpeedStore {
    db: Db,
}

impl SpeedStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// 记录一轮对话：算出派生速度后转发到 DB 写入队列（非阻塞）。
    pub fn record(&self, t: Turn, recorder: &super::db::Recorder) {
        let r = derive(&t);
        let day = r.hour.get(..10).unwrap_or("").to_string();
        recorder.speed(super::db::SpeedEvent {
            ts_ms: chrono::Local::now().timestamp_millis(),
            hour: r.hour,
            day,
            upstream: r.upstream,
            model: r.model,
            stream: r.stream,
            prompt_tokens: r.prompt_tokens,
            cached_tokens: r.cached_tokens,
            completion_tokens: r.completion_tokens,
            ttft_ms: r.ttft_ms,
            duration_ms: r.duration_ms,
            gen_ms: r.gen_ms,
            prefill_tps: r.prefill_tps,
            prefill_fresh_tps: r.prefill_fresh_tps,
            decode_tps: r.decode_tps,
        });
    }

    /// 查询：窗口内的小时序列 + (上游,模型) 汇总 + 该模型小时序列 + 逐轮记录。
    pub async fn query(&self, hours: usize, recent: usize) -> Result<Value, String> {
        let hours = hours.clamp(1, 24 * 30);
        let cutoff = (chrono::Local::now() - chrono::Duration::hours(hours as i64 - 1))
            .format("%Y-%m-%d %H")
            .to_string();
        let rows = self.db.speed_rows(&cutoff).await?;

        // 转为聚合用样本
        let samples: Vec<TurnSample> = rows
            .iter()
            .map(|r| TurnSample {
                upstream: r.upstream.clone(),
                model: r.model.clone(),
                prompt_tokens: r.prompt_tokens,
                completion_tokens: r.completion_tokens,
                cached_tokens: r.cached_tokens,
                prefill_tps: r.prefill_tps,
                prefill_fresh_tps: r.prefill_fresh_tps,
                decode_tps: r.decode_tps,
            })
            .collect();

        // 按小时分桶
        let mut hours_map: BTreeMap<String, HourSpeedAgg> = BTreeMap::new();
        for (r, s) in rows.iter().zip(samples.iter()) {
            hours_map.entry(r.hour.clone()).or_default().add(s);
        }

        // 跨上游/模型总累加器 + (上游,模型) 聚合 + 按天
        let mut window_total = ModelSpeedAgg::default();
        let mut per_um: BTreeMap<(String, String), ModelSpeedAgg> = BTreeMap::new();
        let mut per_day: BTreeMap<String, ModelSpeedAgg> = BTreeMap::new();
        for (hour, agg) in &hours_map {
            let total = agg.total();
            window_total.merge(&total);
            for (um, a) in &agg.models {
                per_um.entry(um.clone()).or_default().merge(a);
            }
            let day = hour.get(..10).unwrap_or("").to_string();
            per_day.entry(day).or_default().merge(&total);
        }

        // 小时序列（整体）
        let hourly: Vec<Value> = hours_map
            .iter()
            .map(|(k, agg)| {
                let t = agg.total();
                json!({
                    "hour": k,
                    "turns": t.turns,
                    "avg_prefill_tps": t.avg_prefill(),
                    "avg_prefill_fresh_tps": t.avg_prefill_fresh(),
                    "avg_decode_tps": t.avg_decode(),
                    "prompt_tokens": t.prompt_tokens,
                    "cached_tokens": t.cached_tokens,
                    "completion_tokens": t.completion_tokens,
                })
            })
            .collect();

        // 小时序列 × (上游, 模型)：供右侧「选中模型」统计
        let mut hourly_by_model: BTreeMap<(String, String), Vec<Value>> = BTreeMap::new();
        for (hour, agg) in &hours_map {
            for (um, a) in &agg.models {
                hourly_by_model.entry(um.clone()).or_default().push(json!({
                    "hour": hour,
                    "turns": a.turns,
                    "avg_prefill_tps": a.avg_prefill(),
                    "avg_prefill_fresh_tps": a.avg_prefill_fresh(),
                    "avg_decode_tps": a.avg_decode(),
                    "prompt_tokens": a.prompt_tokens,
                    "cached_tokens": a.cached_tokens,
                    "completion_tokens": a.completion_tokens,
                }));
            }
        }
        let models_series: Vec<Value> = hourly_by_model
            .into_iter()
            .map(|((up, m), series)| json!({ "upstream": up, "model": m, "hourly": series }))
            .collect();

        // 天序列
        let daily: Vec<Value> = per_day
            .iter()
            .map(|(d, t)| {
                json!({
                    "date": d,
                    "turns": t.turns,
                    "avg_prefill_tps": t.avg_prefill(),
                    "avg_prefill_fresh_tps": t.avg_prefill_fresh(),
                    "avg_decode_tps": t.avg_decode(),
                    "prompt_tokens": t.prompt_tokens,
                    "cached_tokens": t.cached_tokens,
                    "completion_tokens": t.completion_tokens,
                })
            })
            .collect();

        // 模型榜（按 (upstream, model) 聚合，按轮数降序）
        let mut models: Vec<Value> = per_um
            .iter()
            .map(|((up, m), t)| {
                json!({
                    "upstream": up,
                    "model": m,
                    "turns": t.turns,
                    "avg_prefill_tps": t.avg_prefill(),
                    "avg_prefill_fresh_tps": t.avg_prefill_fresh(),
                    "avg_decode_tps": t.avg_decode(),
                    "prompt_tokens": t.prompt_tokens,
                    "cached_tokens": t.cached_tokens,
                    "completion_tokens": t.completion_tokens,
                })
            })
            .collect();
        models.sort_by(|a, b| {
            b["turns"]
                .as_u64()
                .unwrap_or(0)
                .cmp(&a["turns"].as_u64().unwrap_or(0))
        });

        // 逐轮记录（最近 N 条，新的在前）
        let recent_rows = self.db.speed_recent(recent.clamp(1, 2000)).await?;
        let turns: Vec<Value> = recent_rows
            .iter()
            .map(|s| {
                json!({
                    "time": s.time,
                    "hour": s.hour,
                    "upstream": s.upstream,
                    "model": s.model,
                    "stream": s.stream,
                    "prompt_tokens": s.prompt_tokens,
                    "cached_tokens": s.cached_tokens,
                    "completion_tokens": s.completion_tokens,
                    "ttft_ms": s.ttft_ms,
                    "duration_ms": s.duration_ms,
                    "gen_ms": s.gen_ms,
                    "prefill_tps": s.prefill_tps,
                    "prefill_fresh_tps": s.prefill_fresh_tps,
                    "decode_tps": s.decode_tps,
                })
            })
            .collect();

        Ok(json!({
            "window_hours": hours,
            "summary": {
                "turns": window_total.turns,
                "avg_prefill_tps": window_total.avg_prefill(),
                "avg_prefill_fresh_tps": window_total.avg_prefill_fresh(),
                "avg_decode_tps": window_total.avg_decode(),
                "prompt_tokens": window_total.prompt_tokens,
                "cached_tokens": window_total.cached_tokens,
                "completion_tokens": window_total.completion_tokens,
            },
            "hourly": hourly,
            "daily": daily,
            "models": models,
            "models_series": models_series,
            "turns": turns,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个临时文件 SQLite，返回 (查询器, 写入句柄, 目录)。
    ///
    /// 用文件库而非 `:memory:`：连接池下 `:memory:` 每条连接各持一份库，
    /// 写入任务与查询会看到不同实例。
    async fn setup(tag: &str) -> (SpeedStore, super::super::db::Recorder, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("c2a-speed-{tag}-{}", crate::rand_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::init(&dir).await.unwrap();
        let recorder = super::super::db::Recorder::spawn(db.connection());
        (SpeedStore::new(db), recorder, dir)
    }

    /// 记录一轮（默认上游 trae / 模型 m1 / 流式，无缓存命中）。
    fn rec(s: &SpeedStore, r: &super::super::db::Recorder, model: &str, pt: u64, ct: u64, ttft: u64, dur: u64) {
        rec_cached(s, r, model, pt, ct, 0, ttft, dur);
    }

    fn rec_cached(
        s: &SpeedStore,
        r: &super::super::db::Recorder,
        model: &str,
        pt: u64,
        ct: u64,
        cached: u64,
        ttft: u64,
        dur: u64,
    ) {
        s.record(
            Turn {
                upstream: "trae".to_string(),
                model: model.to_string(),
                stream: true,
                prompt_tokens: pt,
                completion_tokens: ct,
                cached_tokens: cached,
                ttft_ms: ttft,
                duration_ms: dur,
            },
            r,
        );
    }

    #[tokio::test]
    async fn test_prefill_and_decode_tps_math() {
        let (s, r, dir) = setup("math").await;
        // TTFT=2s 处理 1000 prompt tokens → 500 tps
        // 生成阶段 = 10s - 2s = 8s，输出 400 tokens → 50 tps
        rec(&s, &r, "m1", 1000, 400, 2000, 10_000);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        assert_eq!(q["summary"]["turns"], 1);
        assert_eq!(q["summary"]["avg_prefill_tps"], 500.0);
        assert_eq!(q["summary"]["avg_decode_tps"], 50.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生成窗口过小（上游整体缓冲）时不应产出假速度。
    #[tokio::test]
    async fn test_tiny_decode_window_yields_no_speed() {
        let (s, r, dir) = setup("tinywin").await;
        // TTFT=3277ms、总耗时 3279ms → 生成窗口仅 2ms，却有 532 tokens
        rec(&s, &r, "m1", 300_000, 532, 3277, 3279);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        assert_eq!(
            q["summary"]["avg_decode_tps"], 0.0,
            "2ms 窗口属于缓冲噪声，不应算出 266000 tps"
        );
        assert_eq!(q["turns"][0]["gen_ms"], 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_cached_tokens_split_prefill() {
        let (s, r, dir) = setup("cached").await;
        // 100000 输入中 95000 命中缓存：整体 50000 tps，实际未命中部分 2500 tps
        rec_cached(&s, &r, "m1", 100_000, 200, 95_000, 2000, 5000);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        assert_eq!(q["summary"]["avg_prefill_tps"], 50000.0);
        assert_eq!(q["summary"]["avg_prefill_fresh_tps"], 2500.0);
        assert_eq!(q["summary"]["cached_tokens"], 95_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_zero_tokens_yield_zero_tps() {
        let (s, r, dir) = setup("zero").await;
        // 上游未返回 token 数 → 速度不可计算，记 0 而非 inf/NaN
        rec(&s, &r, "m1", 0, 0, 100, 500);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        assert_eq!(q["summary"]["avg_prefill_tps"], 0.0);
        assert_eq!(q["summary"]["avg_decode_tps"], 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_average_is_per_turn_equal_weight() {
        let (s, r, dir) = setup("avg").await;
        // 两轮：500 tps 与 100 tps → 平均 300（不是按 token 加权）
        rec(&s, &r, "m1", 1000, 100, 2000, 3000);
        rec(&s, &r, "m1", 100, 100, 1000, 2000);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        let avg = q["summary"]["avg_prefill_tps"].as_f64().unwrap();
        assert!((avg - 300.0).abs() < 0.01, "avg={avg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_hourly_and_daily_buckets_present() {
        let (s, r, dir) = setup("bucket").await;
        rec(&s, &r, "m1", 500, 200, 1000, 5000);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        assert_eq!(q["hourly"].as_array().unwrap().len(), 1);
        assert_eq!(q["daily"].as_array().unwrap().len(), 1);
        assert_eq!(q["hourly"][0]["turns"], 1);
        assert_eq!(q["daily"][0]["turns"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_recent_newest_first_and_limited() {
        let (s, r, dir) = setup("recent").await;
        for _ in 0..5 {
            rec(&s, &r, "m1", 10, 5, 100, 500);
        }
        r.flush().await;
        let q = s.query(24, 3).await.unwrap();
        assert_eq!(q["turns"].as_array().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_model_breakdown_aggregates() {
        let (s, r, dir) = setup("models").await;
        rec(&s, &r, "m1", 1000, 400, 2000, 10_000);
        rec(&s, &r, "m1", 1000, 400, 2000, 10_000);
        rec(&s, &r, "m2", 500, 100, 1000, 3000);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        let models = q["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["model"], "m1");
        assert_eq!(models[0]["turns"], 2);
        assert_eq!(q["summary"]["turns"], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 跨重启持久化：重新打开同一目录仍能查到记录。
    #[tokio::test]
    async fn test_persistence_roundtrip() {
        let dir = std::env::temp_dir().join(format!("c2a-speed-persist-{}", crate::rand_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        {
            let db = Db::init(&dir).await.unwrap();
            let recorder = super::super::db::Recorder::spawn(db.connection());
            let s = SpeedStore::new(db);
            rec(&s, &recorder, "m1", 1000, 400, 2000, 10_000);
            recorder.flush().await;
        }
        // 重新打开
        let db2 = Db::init(&dir).await.unwrap();
        let s2 = SpeedStore::new(db2);
        let q = s2.query(24, 10).await.unwrap();
        assert_eq!(q["summary"]["turns"], 1);
        assert_eq!(q["summary"]["avg_prefill_tps"], 500.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_ttft_exceeds_duration_is_clamped() {
        let (s, r, dir) = setup("clamp").await;
        // 病态数据：TTFT > 总耗时 → 生成阶段归零，不应 panic 或产生 inf
        rec(&s, &r, "m1", 100, 50, 9000, 5000);
        r.flush().await;
        let q = s.query(24, 10).await.unwrap();
        assert_eq!(q["summary"]["avg_decode_tps"], 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}