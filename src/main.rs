//! agents2api — 把 CodeBuddy / WorkBuddy / Trae 的订阅暴露成标准
//! OpenAI / Anthropic 兼容 API。
//!
//! 原理（直连后端，原生 function calling）：
//!   - 读取本机已登录的桌面端凭据（CodeBuddy 读 auth `.info`；Trae 解密 storage.json）。
//!   - 转发到对应后端，并做协议转换 + 鉴权头注入。
//!   - CodeBuddy 后端本身就是 OpenAI chat/completions 协议；Trae 是私有协议，
//!     由 `upstream::trae` 做双向转换。
//!   - 多上游并存，按路径前缀隔离：`/trae/v1/...`、`/codebuddy/v1/...`。

mod admin;
mod anthropic;
mod auth;
mod config;
mod desensitize;
mod identity;
mod models;
mod projection;
mod responses_adapter;
mod routes;
mod upstream;
mod wbcrypto;

use clap::Parser;
use config::{AppConfig, AppState, SharedState};
use std::sync::Arc;

/// n 字节随机数的十六进制表示（2n 个字符）。
pub fn rand_hex(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Parser, Debug)]
#[command(
    name = "agents2api",
    version,
    about = "CodeBuddy / Trae -> OpenAI 兼容转换器（直连后端）"
)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    /// 可选：要求客户端携带的 API key（默认不校验）
    #[arg(long, env = "CODEBUDDY2OPENAI_KEY", default_value = "")]
    api_key: String,
    /// 开启日志并写到该文件；不传则回退读 CODEBUDDY2OPENAI_LOG
    #[arg(long, env = "CODEBUDDY2OPENAI_LOG")]
    log: Option<String>,
    /// 按日期分目录的滚动日志根目录（DIR/<日期>/converter-NNN.log，
    /// 单文件上限 100MB，超过自动滚动）；与 --log 同时给出时 --log 优先
    #[arg(long, env = "CODEBUDDY2OPENAI_LOG_DIR")]
    log_dir: Option<String>,
    /// 启用脱敏：对 system 消息里的合规模板敏感词插入零宽空格
    #[arg(long, default_value_t = false)]
    desensitize: bool,
    /// 配合 --desensitize 使用：跳过 system/harness 压缩，仅做零宽脱敏
    #[arg(long, default_value_t = false)]
    no_compact: bool,
    /// 跳过启动预检
    #[arg(long, default_value_t = false)]
    skip_check: bool,
    /// 无路径前缀时使用的默认上游（codebuddy / trae）
    #[arg(long, default_value = "buddy")]
    upstream: String,
    /// 禁用某个上游（可重复），如 --disable-upstream trae
    #[arg(long = "disable-upstream")]
    disable_upstream: Vec<String>,
    /// 覆盖 CodeBuddy 后端地址
    #[arg(long, env = "CODEBUDDY_BACKEND")]
    codebuddy_backend: Option<String>,
    /// 覆盖 Trae 后端 chat host
    #[arg(long, env = "TRAE_CHAT_HOST")]
    trae_chat_host: Option<String>,
    /// 显式指定 Trae storage.json 路径
    #[arg(long, env = "TRAE_STORAGE_JSON")]
    trae_storage_json: Option<String>,
    /// 显式指定 OpenCode auth.json 路径
    #[arg(long, env = "OPENCODE_AUTH_JSON")]
    opencode_auth_json: Option<String>,
    /// OpenCode Zen 匿名免费通道 base URL 覆盖（默认 https://opencode.ai/zen/v1）
    #[arg(long, env = "OPENCODE_ZEN_BASE_URL")]
    opencode_zen_base_url: Option<String>,
    /// 数据目录（统计、签到记录、后台配置）；默认程序目录下 data/（不可写时回退 %APPDATA%\agents2api）
    #[arg(long, env = "AGENTS2API_DATA_DIR")]
    data_dir: Option<String>,
    /// 关闭管理后台（/admin/*）
    #[arg(long, default_value_t = false)]
    no_admin: bool,
    /// 只输出到控制台，不写文件日志（默认写「程序目录/logs」）
    #[arg(long, default_value_t = false)]
    no_log: bool,
    /// 日志等级：error / warn / info / debug（默认）/ trace（输出请求响应体等逐字明细）
    #[arg(long, env = "LOG_LEVEL", default_value = "debug")]
    log_level: String,
}

/// 默认日志目录：程序目录下的 logs/；不可写（如安装到只读位置）时回退数据目录。
fn program_logs_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("logs");
            if std::fs::create_dir_all(&candidate).is_ok() {
                // 验证可写
                let probe = candidate.join(".write-test");
                if std::fs::write(&probe, b"ok").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return candidate;
                }
            }
        }
    }
    data_dir.join("logs")
}

/// 从 CLI 提取重建注册表所需的基础设置。
fn base_settings(cli: &Cli) -> admin::config_store::BaseSettings {    admin::config_store::BaseSettings {
        codebuddy_backend: cli.codebuddy_backend.clone(),
        trae_chat_host: cli.trae_chat_host.clone(),
        trae_storage_json: cli.trae_storage_json.clone(),
        opencode_auth_json: cli.opencode_auth_json.clone(),
        opencode_zen_base_url: cli.opencode_zen_base_url.clone(),
    }
}

fn preflight(state: &AppState) {
    let info = |m: &str| config::log_at(config::Level::Info, m);
    info("==== 预检 ====");
    info(&format!("平台      : {}", std::env::consts::OS));
    info(&format!("运行时    : rust/{}", env!("CARGO_PKG_VERSION")));
    info(&format!("默认上游  : {}", state.default_upstream()));

    let mut names: Vec<String> = state
        .upstreams_snapshot()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    names.sort();
    if names.is_empty() {
        config::log_at(
            config::Level::Warn,
            "没有可用的上游：未找到任何桌面端登录态。",
        );
        info(&format!(
            "       · CodeBuddy: {}",
            auth::auth_dirs()
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        info(&format!(
            "       · Trae:      {}",
            upstream::trae::auth::storage_json_candidates()
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        info(&format!("可用上游  : {names:?}"));
    }

    if let Some(af) = auth::find_auth_file() {
        info(&format!("CodeBuddy : {}", af.display()));
    }
    if let Some(sf) = upstream::trae::auth::find_storage_json() {
        info(&format!("Trae      : {}", sf.display()));
    }
    if let Some(of) = upstream::opencode::auth::find_auth_json() {
        info(&format!("OpenCode  : {}", of.display()));
    }
    info("================");
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();

    let base = base_settings(&cli);
    // 日志等级最先生效（早于任何日志输出）；非法值回落 debug 并提示
    if !config::set_level(&cli.log_level) {
        config::log(&format!(
            "未知日志等级 {:?}，回退 debug（可选 error/warn/info/debug/trace）",
            cli.log_level
        ));
    }
    let data_dir = cli
        .data_dir
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(admin::default_data_dir);

    // 日志：--log（单文件）优先；其余情况默认开启目录滚动模式，
    // 位置 = 程序目录/logs（不可写则回退数据目录/logs），--log-dir 可覆盖位置
    let resolved_log_dir = cli
        .log_dir
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| program_logs_dir(&data_dir));
    let log_target = if cli.no_log {
        config::LogTarget::None
    } else if let Some(p) = cli.log.clone() {
        config::LogTarget::Single(std::path::PathBuf::from(p))
    } else {
        config::LogTarget::RotatingDir(resolved_log_dir.clone())
    };
    config::init_logger(log_target);
    let log_dir_active = if cli.no_log {
        None
    } else {
        Some(resolved_log_dir.clone())
    };

    // 事件记录数据库（SQLite）：打开/建表，失败则回退内存库（仅本次运行有效）
    let db = match admin::db::Db::init(&data_dir).await {
        Ok(db) => db,
        Err(e) => {
            config::log_at(
                config::Level::Warn,
                &format!("打开记录数据库失败（{e}），本次运行记录不落盘"),
            );
            admin::db::Db::init(&data_dir.join(".tmp-db"))
                .await
                .expect("无法创建临时记录数据库")
        }
    };
    let recorder = admin::db::Recorder::spawn(db.connection());
    // 启动清理过期记录
    if let Err(e) = admin::db::prune(db.conn()).await {
        config::log_at(config::Level::Warn, &format!("清理历史记录失败: {e}"));
    }
    let metrics = Arc::new(admin::MetricsStore::new(db.clone()));
    let speed = Arc::new(admin::speed::SpeedStore::new(db.clone()));
    // 启动即合并文件覆盖项（<data_dir>/upstreams.json），CLI 默认上游仅作文件缺省兜底
    let mut cfg = admin::config_store::UpstreamsConfig::load(&data_dir);
    if cfg.default_upstream.is_none() {
        cfg.default_upstream = Some(cli.upstream.clone());
    }
    let registry = admin::config_store::build_registry(&base, &cli.disable_upstream, &cfg);
    let state: SharedState = Arc::new(AppState {
        config: AppConfig {
            api_key: cli.api_key.clone(),
            desensitize: cli.desensitize,
            no_compact: cli.no_compact,
        },
        upstreams: registry,
        registry_base: base,
        cli_default_upstream: cli.upstream.clone(),
        http: reqwest::Client::builder()
            .build()
            .expect("构建 HTTP 客户端失败"),
        metrics: metrics.clone(),
        speed: speed.clone(),
        db: db.clone(),
        recorder: recorder.clone(),
        data_dir: data_dir.clone(),
        admin_enabled: !cli.no_admin,
        log_dir: log_dir_active.clone(),
    });

    // 每日清理过期记录（应用运行期间每天触发一次）
    {
        let db = db.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let _ = admin::db::prune(db.conn()).await;
            }
        });
    }

    // 载入上次同步的模型目录快照（models() 立即消费；参考实现：网关启动时也自动同步一次）
    crate::upstream::codebuddy_catalog::load_catalog_snapshot(&data_dir);
    crate::upstream::trae::catalog::load_catalog_snapshot(&data_dir);

    // 每日定时模型同步 + 自动签到（应用运行期间自动触发）
    {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let settings = admin::routes::AdminSettings::load(&st.data_dir);
                let now = chrono::Local::now();
                let hhmm = now.format("%H:%M").to_string();
                let today = now.format("%Y-%m-%d").to_string();

                // 每日定时模型同步
                if settings.daily_sync_enabled {
                    let has_syncable = st.upstream(Some("buddy")).is_some()
                        || st.upstream(Some("trae")).is_some();
                    if hhmm == settings.daily_sync_time
                        && settings.last_sync_date != today
                        && has_syncable
                    {
                        // 记录日期在前：失败也视为已尝试，明天再试
                        let mut s = settings.clone();
                        s.last_sync_date = today.clone();
                        if s.save(&st.data_dir).is_err() {
                            continue;
                        }
                        config::log_at(config::Level::Info, "⏰ 每日定时模型同步触发");
                        let admin_state = crate::admin::AdminState {
                            app: st.clone(),
                            data_dir: st.data_dir.clone(),
                            db: st.db.clone(),
                            recorder: st.recorder.clone(),
                        };
                        let _ = admin::routes::sync_models_inner(&admin_state).await;
                    }
                }

                // 自动签到：命中所选时间点且今日该点未触发
                if settings.checkin_auto_enabled && settings.checkin_times.contains(&hhmm) {
                    let marker = format!("{today} {hhmm}");
                    if !settings.last_checkin_fired.contains(&marker) {
                        let mut s = settings.clone();
                        s.last_checkin_fired.push(marker);
                        s.last_checkin_fired.retain(|m| m.starts_with(&today));
                        if s.save(&st.data_dir).is_err() {
                            continue;
                        }
                        config::log_at(
                            config::Level::Info,
                            &format!("⏰ 自动签到触发（{hhmm}）"),
                        );
                        let admin_state = crate::admin::AdminState {
                            app: st.clone(),
                            data_dir: st.data_dir.clone(),
                            db: st.db.clone(),
                            recorder: st.recorder.clone(),
                        };
                        let _ = admin::routes::run_checkin_all(&admin_state).await;
                    }
                }
            }
        });
    }

    if !cli.skip_check {
        preflight(&state);
    }

    config::log_at(
        config::Level::Info,
        &format!(
            "✅ 监听 http://{}:{}（默认上游：{}）",
            cli.host, cli.port, cli.upstream
        ),
    );
    let banner = |m: &str| config::log_at(config::Level::Info, m);
    banner(&format!("   日志等级  : {}", config::level().name()));
    if cli.no_log {
        banner("   文件日志  : 已关闭（仅控制台）");
    } else if let Some(p) = &cli.log {
        banner(&format!("   日志文件  : {p}"));
    } else {
        banner(&format!(
            "   日志目录  : {}/<日期>/converter-NNN.log（单文件 100MB 滚动）",
            resolved_log_dir.display()
        ));
    }
    banner("   GET  /v1/models");
    banner("   POST /v1/chat/completions   (原生 tools/tool_calls，支持流式)");
    banner("   POST /v1/responses          (Responses API，Codex CLI 兼容)");
    banner("   POST /v1/messages           (Anthropic API，Claude Code / CC Switch 兼容)");
    banner("   GET  /health");
    banner("   多上游前缀：/<upstream>/v1/... 与 /<upstream>/health");
    if !cli.api_key.is_empty() {
        banner("   鉴权已启用（API key 已设置）");
    }
    if cli.desensitize {
        let mode = if cli.no_compact {
            "零宽脱敏 + 保留全文"
        } else {
            "零宽脱敏 + 压缩摘要"
        };
        banner(&format!("   脱敏      : 已启用（{mode}）"));
    }
    config::log("按 Ctrl+C 退出。");

    config::log("==== converter 启动 ====");

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind((cli.host.as_str(), cli.port))
        .await
        .unwrap_or_else(|e| panic!("绑定 {}:{} 失败: {e}", cli.host, cli.port));
    axum::serve(listener, app).await.expect("服务器运行失败");
}
