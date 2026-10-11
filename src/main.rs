//! agents2api — 把 CodeBuddy / WorkBuddy / Trae 的订阅暴露成标准
//! OpenAI / Anthropic 兼容 API。
//!
//! 原理（直连后端，原生 function calling）：
//!   - 读取本机已登录的桌面端凭据（CodeBuddy 读 auth `.info`；Trae 解密 storage.json）。
//!   - 转发到对应后端，并做协议转换 + 鉴权头注入。
//!   - CodeBuddy 后端本身就是 OpenAI chat/completions 协议；Trae 是私有协议，
//!     由 `upstream::trae` 做双向转换。
//!   - 多上游并存，按路径前缀隔离：`/trae/v1/...`、`/codebuddy/v1/...`。

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
use std::collections::HashMap;
use std::sync::Arc;
use upstream::codebuddy::CodeBuddyUpstream;
use upstream::trae::{auth::TraeCredential, TraeUpstream};
use upstream::Upstream;

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
    #[arg(long, default_value = "codebuddy")]
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
}

/// 构建上游注册表。
fn build_upstreams(cli: &Cli) -> HashMap<String, Arc<dyn Upstream>> {
    let mut map: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    let disabled = |name: &str| cli.disable_upstream.iter().any(|d| d == name);

    // CodeBuddy / WorkBuddy 是同一个上游：二者共用登录态文件与后端
    // （auth 目录为 CodeBuddyExtension，优先读 workbuddy-desktop.info）。
    // 两个名字都注册，避免用户按自己安装的客户端名调用时 404。
    if !disabled("codebuddy") && !disabled("workbuddy") {
        if let Some(path) = auth::find_auth_file() {
            let cred = auth::CredentialManager::new(path);
            let inst: Arc<dyn Upstream> = Arc::new(CodeBuddyUpstream::new(
                cred,
                cli.codebuddy_backend.clone(),
            ));
            map.insert("codebuddy".to_string(), inst.clone());
            map.insert("workbuddy".to_string(), inst);
        }
    }

    // Trae
    if !disabled("trae") {
        let storage = cli
            .trae_storage_json
            .clone()
            .map(std::path::PathBuf::from)
            .or_else(upstream::trae::auth::find_storage_json);
        if let Some(path) = storage {
            map.insert(
                "trae".to_string(),
                Arc::new(TraeUpstream::new(
                    TraeCredential::new(path),
                    cli.trae_chat_host.clone(),
                )),
            );
        }
    }

    map
}

fn preflight(state: &AppState) {
    eprintln!("==== 预检 ====");
    eprintln!("平台      : {}", std::env::consts::OS);
    eprintln!("运行时    : rust/{}", env!("CARGO_PKG_VERSION"));
    eprintln!("默认上游  : {}", state.default_upstream);

    let mut names: Vec<&String> = state.upstreams.keys().collect();
    names.sort();
    if names.is_empty() {
        eprintln!("[警告] 没有可用的上游：未找到任何桌面端登录态。");
        eprintln!("       · CodeBuddy: {}", auth::auth_dirs()
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", "));
        eprintln!("       · Trae:      {}", upstream::trae::auth::storage_json_candidates()
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", "));
    } else {
        eprintln!("可用上游  : {names:?}");
    }

    if let Some(af) = auth::find_auth_file() {
        eprintln!("CodeBuddy : {}", af.display());
    }
    if let Some(sf) = upstream::trae::auth::find_storage_json() {
        eprintln!("Trae      : {}", sf.display());
    }
    eprintln!("================");
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();

    // --log（单文件）优先于 --log-dir（滚动）；都未给出则只输出控制台
    let log_target = if let Some(p) = cli.log.clone() {
        config::LogTarget::Single(std::path::PathBuf::from(p))
    } else if let Some(d) = cli.log_dir.clone() {
        config::LogTarget::RotatingDir(std::path::PathBuf::from(d))
    } else {
        config::LogTarget::None
    };
    config::init_logger(log_target);

    let upstreams = build_upstreams(&cli);
    let state: SharedState = Arc::new(AppState {
        config: AppConfig {
            api_key: cli.api_key.clone(),
            desensitize: cli.desensitize,
            no_compact: cli.no_compact,
        },
        upstreams,
        default_upstream: cli.upstream.clone(),
        http: reqwest::Client::builder()
            .build()
            .expect("构建 HTTP 客户端失败"),
    });

    if !cli.skip_check {
        preflight(&state);
    }

    eprintln!(
        "\n✅ 监听 http://{}:{}（默认上游：{}）",
        cli.host, cli.port, cli.upstream
    );
    eprintln!("   GET  /v1/models");
    eprintln!("   POST /v1/chat/completions   (原生 tools/tool_calls，支持流式)");
    eprintln!("   POST /v1/responses          (Responses API，Codex CLI 兼容)");
    eprintln!("   POST /v1/messages           (Anthropic API，Claude Code / CC Switch 兼容)");
    eprintln!("   GET  /health");
    eprintln!("   多上游前缀：/<upstream>/v1/... 与 /<upstream>/health");
    if let Some(d) = &cli.log_dir {
        eprintln!(
            "   日志目录  : {d}/<日期>/converter-NNN.log（单文件 100MB 滚动）"
        );
    }
    if !cli.api_key.is_empty() {
        eprintln!("   鉴权已启用（API key 已设置）");
    }
    if cli.desensitize {
        let mode = if cli.no_compact {
            "零宽脱敏 + 保留全文"
        } else {
            "零宽脱敏 + 压缩摘要"
        };
        eprintln!("   脱敏      : 已启用（{mode}）");
    }
    eprintln!("按 Ctrl+C 退出。\n");

    config::log("==== converter 启动 ====");

    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind((cli.host.as_str(), cli.port))
        .await
        .unwrap_or_else(|e| panic!("绑定 {}:{} 失败: {e}", cli.host, cli.port));
    axum::serve(listener, app).await.expect("服务器运行失败");
}
