//! 事件记录持久化（SQLite / SeaORM）。
//!
//! 三类**事件记录**落库（配置类文件仍走 JSON，不在此列）：
//! - `usage_events`：逐请求用量（对应 `MetricsStore`）
//! - `speed_turns`：逐轮吞吐（对应 `SpeedStore`）
//! - `checkin_records`：签到结果
//!
//! 写入路径：请求处理线程只做 `send`（非阻塞），由后台**单写入任务**串行落库——
//! 这样既保证 SQLite 单写者语义，也让 `Drop` 场景（客户端中断时的收尾守卫）
//! 能安全记账（`Drop` 里无法 `await`）。
//!
//! 查询路径：直接 `SELECT` 窗口内原始行，再由各模块用既有聚合逻辑汇总
//! （沿用已测试的 `DayStats` / `ModelSpeedAgg`，避免把聚合重写成 SQL 带来的风险）。

use std::path::Path;
use std::time::Duration;

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement, Value,
};
use tokio::sync::mpsc;

/// 用量/吞吐记录保留天数。
const RETENTION_DAYS: i64 = 90;

/// 一条用量事件（对应 [`crate::admin::metrics::Record`]）。
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub ts_ms: i64,
    pub day: String,
    pub upstream: String,
    pub model: String,
    pub stream: bool,
    pub ok: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u64,
    pub ttfb_ms: u64,
}

/// 一条吞吐轮次事件（对应 [`crate::admin::speed::Turn`]）。
#[derive(Debug, Clone)]
pub struct SpeedEvent {
    pub ts_ms: i64,
    pub hour: String,
    pub day: String,
    pub upstream: String,
    pub model: String,
    pub stream: bool,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub ttft_ms: u64,
    pub duration_ms: u64,
    pub gen_ms: u64,
    pub prefill_tps: f64,
    pub prefill_fresh_tps: f64,
    pub decode_tps: f64,
}

/// 一条签到记录（对应 [`crate::admin::checkin::CheckinRecord`]）。
#[derive(Debug, Clone)]
pub struct CheckinEvent {
    pub ts_ms: i64,
    pub date: String,
    pub time: String,
    pub uid: String,
    pub status: String,
    pub message: String,
    pub credits: f64,
    pub delta: f64,
    pub upstream: String,
}

/// 待落库的事件。
enum Event {
    Usage(UsageEvent),
    Speed(SpeedEvent),
    Checkin(CheckinEvent),
    /// 屏障：此前所有事件落库后回执（供优雅退出与测试同步）。
    #[allow(dead_code)]
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// 查询返回的用量行。
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub day: String,
    pub upstream: String,
    pub model: String,
    pub stream: bool,
    pub ok: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub duration_ms: u64,
    pub ttfb_ms: u64,
}

/// 查询返回的吞吐轮次行。
#[derive(Debug, Clone)]
pub struct SpeedRow {
    pub time: String,
    pub hour: String,
    pub upstream: String,
    pub model: String,
    pub stream: bool,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub ttft_ms: u64,
    pub duration_ms: u64,
    pub gen_ms: u64,
    pub prefill_tps: f64,
    pub prefill_fresh_tps: f64,
    pub decode_tps: f64,
}

/// 查询返回的签到行。
#[derive(Debug, Clone)]
pub struct CheckinRow {
    pub date: String,
    pub time: String,
    pub uid: String,
    pub status: String,
    pub message: String,
    pub credits: f64,
    pub delta: f64,
    pub upstream: String,
}

/// 数据库句柄（只读查询 + 建表）。
#[derive(Clone)]
pub struct Db {
    conn: DatabaseConnection,
}

impl Db {
    /// 打开（或创建）`<data_dir>/agents2api.db` 并确保表结构存在。
    pub async fn init(data_dir: &Path) -> Result<Self, String> {
        if let Some(parent) = data_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::create_dir_all(data_dir).map_err(|e| format!("创建数据目录失败: {e}"))?;
        let path = data_dir.join("agents2api.db");
        // SeaORM 的 SQLite URL：Windows 反斜杠需转正斜杠；mode=rwc 允许创建
        let url = format!(
            "sqlite://{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let mut opt = ConnectOptions::new(url);
        opt.max_connections(5)
            .min_connections(1)
            .connect_timeout(Duration::from_secs(30))
            .sqlx_logging(false);
        let conn = Database::connect(opt)
            .await
            .map_err(|e| format!("打开 SQLite 失败（{}）: {e}", path.display()))?;
        // WAL 提升并发读性能；busy_timeout 避免瞬时锁冲突直接报错
        for pragma in [
            "PRAGMA journal_mode=WAL;",
            "PRAGMA busy_timeout=5000;",
            "PRAGMA synchronous=NORMAL;",
        ] {
            conn.execute(Statement::from_string(DbBackend::Sqlite, pragma.to_string()))
                .await
                .map_err(|e| format!("设置 PRAGMA 失败: {e}"))?;
        }
        let db = Self { conn };
        db.create_schema().await?;
        Ok(db)
    }

    async fn create_schema(&self) -> Result<(), String> {
        const DDL: &[&str] = &[
            r#"CREATE TABLE IF NOT EXISTS usage_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                day TEXT NOT NULL,
                upstream TEXT NOT NULL,
                model TEXT NOT NULL,
                stream INTEGER NOT NULL,
                ok INTEGER NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                ttfb_ms INTEGER NOT NULL
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_usage_day ON usage_events(day)",
            r#"CREATE TABLE IF NOT EXISTS speed_turns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                hour TEXT NOT NULL,
                day TEXT NOT NULL,
                upstream TEXT NOT NULL,
                model TEXT NOT NULL,
                stream INTEGER NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                cached_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                ttft_ms INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                gen_ms INTEGER NOT NULL,
                prefill_tps REAL NOT NULL,
                prefill_fresh_tps REAL NOT NULL,
                decode_tps REAL NOT NULL
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_speed_hour ON speed_turns(hour)",
            "CREATE INDEX IF NOT EXISTS idx_speed_day ON speed_turns(day)",
            r#"CREATE TABLE IF NOT EXISTS checkin_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                date TEXT NOT NULL,
                time TEXT NOT NULL,
                uid TEXT NOT NULL,
                status TEXT NOT NULL,
                message TEXT NOT NULL,
                credits REAL NOT NULL,
                delta REAL NOT NULL,
                upstream TEXT NOT NULL
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_checkin_date ON checkin_records(date)",
        ];
        for sql in DDL {
            self.conn
                .execute(Statement::from_string(DbBackend::Sqlite, sql.to_string()))
                .await
                .map_err(|e| format!("建表失败: {e}"))?;
        }
        Ok(())
    }

    /// 窗口内的用量行（`day >= cutoff_day`）。
    pub async fn usage_rows(&self, cutoff_day: &str) -> Result<Vec<UsageRow>, String> {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT day, upstream, model, stream, ok, prompt_tokens, completion_tokens, \
             duration_ms, ttfb_ms FROM usage_events WHERE day >= ? ORDER BY id",
            [Value::from(cutoff_day.to_string())],
        );
        let rows = self
            .conn
            .query_all(stmt)
            .await
            .map_err(|e| format!("查询用量失败: {e}"))?;
        rows.into_iter().map(map_usage_row).collect()
    }

    /// 窗口内的吞吐轮次行（`hour >= cutoff_hour`），按 id 升序。
    pub async fn speed_rows(&self, cutoff_hour: &str) -> Result<Vec<SpeedRow>, String> {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT ts, hour, upstream, model, stream, prompt_tokens, cached_tokens, \
             completion_tokens, ttft_ms, duration_ms, gen_ms, prefill_tps, \
             prefill_fresh_tps, decode_tps FROM speed_turns WHERE hour >= ? ORDER BY id",
            [Value::from(cutoff_hour.to_string())],
        );
        self.query_speed(stmt).await
    }

    /// 最近 N 条吞吐轮次（新的在前），供“轮次记录”列表。
    pub async fn speed_recent(&self, limit: usize) -> Result<Vec<SpeedRow>, String> {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT ts, hour, upstream, model, stream, prompt_tokens, cached_tokens, \
             completion_tokens, ttft_ms, duration_ms, gen_ms, prefill_tps, \
             prefill_fresh_tps, decode_tps FROM speed_turns ORDER BY id DESC LIMIT ?",
            [Value::from(limit as i64)],
        );
        self.query_speed(stmt).await
    }

    async fn query_speed(&self, stmt: Statement) -> Result<Vec<SpeedRow>, String> {
        let rows = self
            .conn
            .query_all(stmt)
            .await
            .map_err(|e| format!("查询吞吐失败: {e}"))?;
        rows.into_iter().map(map_speed_row).collect()
    }

    /// 窗口内的签到行（`date >= cutoff_date`），按 id 升序。
    pub async fn checkin_rows(&self, cutoff_date: &str) -> Result<Vec<CheckinRow>, String> {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT date, time, uid, status, message, credits, delta, upstream \
             FROM checkin_records WHERE date >= ? ORDER BY id",
            [Value::from(cutoff_date.to_string())],
        );
        let rows = self
            .conn
            .query_all(stmt)
            .await
            .map_err(|e| format!("查询签到失败: {e}"))?;
        rows.into_iter().map(map_checkin_row).collect()
    }
}

fn get_i64(r: &sea_orm::QueryResult, col: &str) -> Result<i64, String> {
    r.try_get::<i64>("", col)
        .map_err(|e| format!("读取列 {col} 失败: {e}"))
}

fn get_str(r: &sea_orm::QueryResult, col: &str) -> Result<String, String> {
    r.try_get::<String>("", col)
        .map_err(|e| format!("读取列 {col} 失败: {e}"))
}

fn get_f64(r: &sea_orm::QueryResult, col: &str) -> Result<f64, String> {
    r.try_get::<f64>("", col)
        .map_err(|e| format!("读取列 {col} 失败: {e}"))
}

fn map_usage_row(r: sea_orm::QueryResult) -> Result<UsageRow, String> {
    Ok(UsageRow {
        day: get_str(&r, "day")?,
        upstream: get_str(&r, "upstream")?,
        model: get_str(&r, "model")?,
        stream: get_i64(&r, "stream")? != 0,
        ok: get_i64(&r, "ok")? != 0,
        prompt_tokens: get_i64(&r, "prompt_tokens")?.max(0) as u64,
        completion_tokens: get_i64(&r, "completion_tokens")?.max(0) as u64,
        duration_ms: get_i64(&r, "duration_ms")?.max(0) as u64,
        ttfb_ms: get_i64(&r, "ttfb_ms")?.max(0) as u64,
    })
}

fn map_speed_row(r: sea_orm::QueryResult) -> Result<SpeedRow, String> {
    // 由毫秒时间戳还原 HH:MM:SS（本地时区）
    let ts = get_i64(&r, "ts")?;
    let time = chrono::DateTime::from_timestamp_millis(ts)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_default();
    Ok(SpeedRow {
        time,
        hour: get_str(&r, "hour")?,
        upstream: get_str(&r, "upstream")?,
        model: get_str(&r, "model")?,
        stream: get_i64(&r, "stream")? != 0,
        prompt_tokens: get_i64(&r, "prompt_tokens")?.max(0) as u64,
        cached_tokens: get_i64(&r, "cached_tokens")?.max(0) as u64,
        completion_tokens: get_i64(&r, "completion_tokens")?.max(0) as u64,
        ttft_ms: get_i64(&r, "ttft_ms")?.max(0) as u64,
        duration_ms: get_i64(&r, "duration_ms")?.max(0) as u64,
        gen_ms: get_i64(&r, "gen_ms")?.max(0) as u64,
        prefill_tps: get_f64(&r, "prefill_tps")?,
        prefill_fresh_tps: get_f64(&r, "prefill_fresh_tps")?,
        decode_tps: get_f64(&r, "decode_tps")?,
    })
}

fn map_checkin_row(r: sea_orm::QueryResult) -> Result<CheckinRow, String> {
    Ok(CheckinRow {
        date: get_str(&r, "date")?,
        time: get_str(&r, "time")?,
        uid: get_str(&r, "uid")?,
        status: get_str(&r, "status")?,
        message: get_str(&r, "message")?,
        credits: get_f64(&r, "credits")?,
        delta: get_f64(&r, "delta")?,
        upstream: get_str(&r, "upstream")?,
    })
}

/// 事件记录句柄：`send` 非阻塞，实际落库在后台单任务串行执行。
#[derive(Clone)]
pub struct Recorder {
    tx: mpsc::UnboundedSender<Event>,
}

impl Recorder {
    /// 在给定连接上启动后台写入任务，返回句柄。
    pub fn spawn(conn: DatabaseConnection) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let Err(e) = write_event(&conn, ev).await {
                    crate::config::log_at(
                        crate::config::Level::Warn,
                        &format!("记录落库失败: {e}"),
                    );
                }
            }
        });
        Self { tx }
    }

    /// 记录一次请求用量。
    pub fn usage(&self, ev: UsageEvent) {
        let _ = self.tx.send(Event::Usage(ev));
    }

    /// 记录一轮吞吐。
    pub fn speed(&self, ev: SpeedEvent) {
        let _ = self.tx.send(Event::Speed(ev));
    }

    /// 记录一条签到结果。
    pub fn checkin(&self, ev: CheckinEvent) {
        let _ = self.tx.send(Event::Checkin(ev));
    }

    /// 等待此前入队的事件全部落库（供退出与测试同步）。
    #[allow(dead_code)]
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Event::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

async fn write_event(conn: &DatabaseConnection, ev: Event) -> Result<(), String> {
    match ev {
        Event::Usage(e) => {
            let stmt = Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO usage_events (ts, day, upstream, model, stream, ok, \
                 prompt_tokens, completion_tokens, duration_ms, ttfb_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    Value::from(e.ts_ms),
                    Value::from(e.day),
                    Value::from(e.upstream),
                    Value::from(e.model),
                    Value::from(e.stream as i64),
                    Value::from(e.ok as i64),
                    Value::from(e.prompt_tokens as i64),
                    Value::from(e.completion_tokens as i64),
                    Value::from(e.duration_ms as i64),
                    Value::from(e.ttfb_ms as i64),
                ],
            );
            conn.execute(stmt)
                .await
                .map_err(|e| format!("写入用量失败: {e}"))?;
        }
        Event::Speed(e) => {
            let stmt = Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO speed_turns (ts, hour, day, upstream, model, stream, \
                 prompt_tokens, cached_tokens, completion_tokens, ttft_ms, duration_ms, \
                 gen_ms, prefill_tps, prefill_fresh_tps, decode_tps) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    Value::from(e.ts_ms),
                    Value::from(e.hour),
                    Value::from(e.day),
                    Value::from(e.upstream),
                    Value::from(e.model),
                    Value::from(e.stream as i64),
                    Value::from(e.prompt_tokens as i64),
                    Value::from(e.cached_tokens as i64),
                    Value::from(e.completion_tokens as i64),
                    Value::from(e.ttft_ms as i64),
                    Value::from(e.duration_ms as i64),
                    Value::from(e.gen_ms as i64),
                    Value::from(e.prefill_tps),
                    Value::from(e.prefill_fresh_tps),
                    Value::from(e.decode_tps),
                ],
            );
            conn.execute(stmt)
                .await
                .map_err(|e| format!("写入吞吐失败: {e}"))?;
        }
        Event::Checkin(e) => {
            let stmt = Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO checkin_records (ts, date, time, uid, status, message, \
                 credits, delta, upstream) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    Value::from(e.ts_ms),
                    Value::from(e.date),
                    Value::from(e.time),
                    Value::from(e.uid),
                    Value::from(e.status),
                    Value::from(e.message),
                    Value::from(e.credits),
                    Value::from(e.delta),
                    Value::from(e.upstream),
                ],
            );
            conn.execute(stmt)
                .await
                .map_err(|e| format!("写入签到失败: {e}"))?;
        }
        Event::Flush(ack) => {
            let _ = ack.send(());
        }
    }
    Ok(())
}

/// 删除超过保留期的记录（启动时与每日各调一次）。
pub async fn prune(conn: &DatabaseConnection) -> Result<(), String> {
    let cutoff_day = (chrono::Local::now() - chrono::Duration::days(RETENTION_DAYS))
        .format("%Y-%m-%d")
        .to_string();
    for sql in [
        "DELETE FROM usage_events WHERE day < ?",
        "DELETE FROM speed_turns WHERE day < ?",
        "DELETE FROM checkin_records WHERE date < ?",
    ] {
        let stmt = Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            [Value::from(cutoff_day.clone())],
        );
        conn.execute(stmt)
            .await
            .map_err(|e| format!("清理历史失败: {e}"))?;
    }
    Ok(())
}

/// 暴露底层连接（供启动/维护使用）。
impl Db {
    pub fn conn(&self) -> &DatabaseConnection {
        &self.conn
    }

    /// 克隆连接（`DatabaseConnection` 内部为连接池，克隆廉价）。
    pub fn connection(&self) -> DatabaseConnection {
        self.conn.clone()
    }
}
