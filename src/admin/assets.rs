//! 内嵌前端资源（rust-embed）+ SPA fallback。
//!
//! release 下前端打包进二进制（单文件分发）；debug 下若 `web/dist` 存在则优先读磁盘
//! （配合 `npm run dev` 或 `npm run build` 后刷新即可，无需重编译 Rust）。
//! 未构建前端时 `/admin/` 返回引导页，不影响 `/v1/*`。

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// 内嵌 `web/dist`。目录必须存在（含占位 index.html）才能在编译期通过。
#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Assets;

/// 未构建前端时的引导页。
const BOOTSTRAP_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>agents2api 管理后台</title>
<style>body{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;max-width:640px;margin:80px auto;padding:0 24px;color:#1f2328;line-height:1.6}
code{background:#f3f4f6;padding:2px 6px;border-radius:4px}h1{font-size:20px}</style></head>
<body><h1>agents2api 管理后台</h1>
<p>前端尚未构建。请构建后再访问：</p>
<pre><code>cd web
npm install
npm run build</code></pre>
<p>开发模式下可运行 <code>npm run dev</code>（Vite 会把 <code>/admin/api</code> 代理到本服务）。</p>
<p>API 已可用，例如 <a href="/admin/api/overview"><code>/admin/api/overview</code></a>。</p>
</body></html>"#;

/// 读取资源：debug 模式优先磁盘 `web/dist`，否则用内嵌。
fn read_asset(path: &str) -> Option<(Vec<u8>, String)> {
    // debug 且磁盘存在时优先（便于前端热改）
    if cfg!(debug_assertions) {
        let disk = std::path::Path::new("web/dist").join(path);
        if let Ok(bytes) = std::fs::read(&disk) {
            let mime = mime_guess::from_path(&disk)
                .first_or_octet_stream()
                .to_string();
            return Some((bytes, mime));
        }
    }
    Assets::get(path).map(|f| {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        (f.data.into_owned(), mime)
    })
}

/// 是否存在真正构建过的前端（而非仅占位）。
fn frontend_built() -> bool {
    match read_asset("index.html") {
        Some((bytes, _)) => {
            let text = String::from_utf8_lossy(&bytes);
            // 占位 index.html 含标记，视为未构建
            !text.contains("__AGENTS2API_PLACEHOLDER__")
        }
        None => false,
    }
}

fn serve(path: &str) -> Response {
    if !frontend_built() {
        return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], BOOTSTRAP_HTML)
            .into_response();
    }
    match read_asset(path) {
        Some((bytes, mime)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .body(axum::body::Body::from(bytes))
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// `GET /admin/` — 前端入口。
pub async fn index() -> Response {
    serve("index.html")
}

/// 未知 `/admin/*` 路径：带扩展名的找静态资源（找不到 404），
/// 其余（SPA 前端路由如 /dashboard）一律回 `index.html`。
///
/// 注意：axum 的 fallback 没有路由参数，`Path` 提取器会报错，必须用 `OriginalUri`。
pub async fn spa_fallback(uri: axum::extract::OriginalUri) -> Response {
    // 嵌套路由下 OriginalUri 仍带 /admin 前缀，自行剥掉
    let path = uri
        .path()
        .trim_start_matches("/admin/")
        .trim_start_matches('/')
        .to_string();
    if std::path::Path::new(&path)
        .extension()
        .map(|e| e != "html")
        .unwrap_or(false)
    {
        return match read_asset(&path) {
            Some((bytes, mime)) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(axum::body::Body::from(bytes))
                .unwrap(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }
    serve("index.html")
}
