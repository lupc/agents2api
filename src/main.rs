//! codebuddy2api-rs — 把 CodeBuddy / WorkBuddy 的订阅暴露成标准 OpenAI / Anthropic 兼容 API。
//!
//! 原理（直连后端，原生 function calling）：
//!   - 读取本机已登录的 CodeBuddy 桌面端凭据（auth 文件里的 token / uid / enterpriseId）。
//!   - 直接转发到 CodeBuddy 后端 `https://copilot.tencent.com/v2/chat/completions`
//!     （该后端本身就是标准 OpenAI chat/completions 协议，含原生 tools / tool_calls / SSE 流式）。
//!   - 转换器只做两件事：①注入鉴权 header ②在本地 /v1/* 与后端 /v2/* 之间做协议转换。
//!   - token 过期时自动调 `/v2/plugin/auth/token/refresh` 刷新，并回写 auth 文件。

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
    name = "codebuddy2api",
    about = "CodeBuddy -> OpenAI 兼容转换器（直连后端）"
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
    /// 启用脱敏：对 system 消息里的合规模板敏感词插入零宽空格
    #[arg(long, default_value_t = false)]
    desensitize: bool,
    /// 配合 --desensitize 使用：跳过 system/harness 压缩，仅做零宽脱敏
    #[arg(long, default_value_t = false)]
    no_compact: bool,
    /// 跳过启动预检
    #[arg(long, default_value_t = false)]
    skip_check: bool,
}

fn preflight(cred: Option<&auth::CredentialManager>) -> bool {
    let af = auth::find_auth_file();
    eprintln!("==== 预检 ====");
    eprintln!("平台      : {}", std::env::consts::OS);
    eprintln!("运行时    : rust/{}", env!("CARGO_PKG_VERSION"));
    eprintln!(
        "后端      : {} (直连，原生 function calling)",
        auth::BACKEND
    );
    eprintln!(
        "登录文件  : {}",
        af.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(未找到)".to_string())
    );
    let dirs = auth::auth_dirs();
    eprintln!(
        "已查目录  : {}",
        dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut ok = true;
    if af.is_none() {
        eprintln!("\n[警告] 未找到登录文件。请在桌面端完成登录（CodeBuddy/WorkBuddy）。");
        ok = false;
    } else if let Some(cred) = cred {
        // preflight 在 async 上下文之外调用，这里用阻塞包装读摘要
        let rt = tokio::runtime::Handle::current();
        match tokio::task::block_in_place(|| {
            rt.block_on(cred.summary())
        }) {
            Ok(info) => {
                eprintln!(
                    "账号      : {} / {}",
                    info.get("nickname").and_then(|v| v.as_str()).unwrap_or("?"),
                    info.get("enterpriseName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                );
                eprintln!(
                    "token过期 : {}",
                    if info.get("token_expired").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "是(将自动刷新)"
                    } else {
                        "否"
                    }
                );
            }
            Err(e) => {
                eprintln!("[警告] 读取凭据失败：{e}");
                ok = false;
            }
        }
    }
    eprintln!("================");
    ok
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cli = Cli::parse();

    let log_path = cli.log.clone().map(std::path::PathBuf::from);
    config::init_logger(log_path.clone());

    let af = auth::find_auth_file();
    let cred = af.map(auth::CredentialManager::new);

    if !cli.skip_check {
        preflight(cred.as_ref());
    }

    let config = AppConfig {
        api_key: cli.api_key.clone(),
        desensitize: cli.desensitize,
        no_compact: cli.no_compact,
    };
    let state: SharedState = Arc::new(AppState {
        config,
        cred,
        http: reqwest::Client::builder()
            .build()
            .expect("构建 HTTP 客户端失败"),
    });

    eprintln!(
        "\n✅ 监听 http://{}:{}（直连后端，原生 function calling）",
        cli.host, cli.port
    );
    eprintln!("   GET  /v1/models");
    eprintln!("   POST /v1/chat/completions   (原生 tools/tool_calls，支持流式)");
    eprintln!("   POST /v1/responses          (Responses API，Codex CLI 兼容)");
    eprintln!("   POST /v1/messages           (Anthropic API，Claude Code / CC Switch 兼容)");
    eprintln!("   GET  /health");
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
        .unwrap_or_else(|e| panic!("绑定 {host}:{port} 失败: {e}", host = cli.host, port = cli.port));
    axum::serve(listener, app).await.expect("服务器运行失败");
}
