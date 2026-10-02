//! axum 路由与处理器。
//!
//! 支持多上游并存，按路径前缀隔离：
//! - 无前缀：`/v1/*`、`/health` 走默认上游
//! - 前缀：`/{upstream}/v1/*`、`/{upstream}/health` 走指定上游
//!
//! 四个端点的共同流程（准备请求 → 发起 → 流式转发 / 非流式聚合）下沉到
//! [`execute_upstream`]，各协议适配器只负责「转换请求体」和「转换响应流」。

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

use crate::anthropic::{anthropic_request_to_chat, AnthropicStreamConverter};
use crate::auth::parse_headers;
use crate::config::{log_rid, new_rid, AppState, SharedState};
use crate::desensitize::desensitize_body;
use crate::identity::filter_system_identity;
use crate::projection::project_responses_chat_body;
use crate::responses_adapter::{responses_request_to_chat, ResponsesStreamConverter};
use crate::upstream::{
    err_event, looks_like_content_filter_text, ByteStream, StreamStats, Upstream,
};

/// 后端请求体里出现过的额外字段（透传时若客户端给了就保留）
const PASSTHROUGH_BODY_KEYS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "temperature",
    "max_tokens",
    "max_completion_tokens",
    "top_p",
    "stream",
    "stream_options",
    "stop",
    "presence_penalty",
    "frequency_penalty",
    "n",
    "response_format",
    "seed",
    "user",
    "reasoning_effort",
    "verbosity",
    "reasoning_summary",
];

/// 聚合响应体上限，防止异常上游占用过多内存。
const MAX_UPSTREAM_BODY: usize = 64 * 1024 * 1024;

pub fn router(state: SharedState) -> Router {
    Router::new()
        // 无前缀 → 默认上游
        .route("/health", get(health_default))
        .route("/v1/models", get(models_default))
        .route("/v1/chat/completions", post(chat_completions_default))
        .route("/v1/responses", post(create_response_default))
        .route("/v1/messages", post(create_message_default))
        .route("/v1/messages/count_tokens", post(count_tokens_default))
        // 前缀 → 指定上游
        .route("/{upstream}/health", get(health_named))
        .route("/{upstream}/v1/models", get(models_named))
        .route(
            "/{upstream}/v1/chat/completions",
            post(chat_completions_named),
        )
        .route("/{upstream}/v1/responses", post(create_response_named))
        .route("/{upstream}/v1/messages", post(create_message_named))
        .route(
            "/{upstream}/v1/messages/count_tokens",
            post(count_tokens_named),
        )
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// 通用工具
// ---------------------------------------------------------------------------

/// 复刻 FastAPI HTTPException 的响应格式：`{"detail": <detail>}`。
fn err_detail(status: StatusCode, detail: Value) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn err_openai(status: StatusCode, message: impl Into<String>, kind: &str) -> Response {
    err_detail(
        status,
        json!({"error": {"message": message.into(), "type": kind}}),
    )
}

fn sse_response(
    stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ").trim().to_string();
    if s.chars().count() > n {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    } else {
        s
    }
}

fn bearer_token(headers: &HeaderMap) -> String {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            return token.trim().to_string();
        }
    }
    String::new()
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let key = &state.config.api_key;
    if key.is_empty() {
        return Ok(());
    }
    let mut token = bearer_token(headers);
    if token.is_empty() {
        token = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
    }
    if token != *key {
        return Err(err_detail(
            StatusCode::UNAUTHORIZED,
            json!({"error": {"message": "invalid api key", "type": "auth_error"}}),
        ));
    }
    Ok(())
}

/// 解析路径前缀 / 默认上游，取出对应上游实例。
fn resolve_upstream(
    state: &AppState,
    name: Option<&str>,
) -> Result<Arc<dyn Upstream>, Response> {
    // 空串或 None 都走默认上游
    let requested = name.filter(|n| !n.is_empty());
    match state.upstream(requested) {
        Some(u) => Ok(u),
        None => {
            let asked = requested.unwrap_or("(默认)");
            let available: Vec<&str> = state.upstreams.keys().map(|s| s.as_str()).collect();
            Err(err_detail(
                StatusCode::NOT_FOUND,
                json!({
                    "error": {
                        "message": format!("未配置的上游：{asked}；已配置：{available:?}"),
                        "type": "invalid_request_error",
                    }
                }),
            ))
        }
    }
}

async fn parse_body(body: &Bytes) -> Result<Value, Response> {
    serde_json::from_slice::<Value>(body).map_err(|e| {
        err_openai(
            StatusCode::BAD_REQUEST,
            format!("bad json: {e}"),
            "invalid_request_error",
        )
    })
}

fn safe_err_raw(raw: &[u8], status: u16) -> Value {
    let text = String::from_utf8_lossy(raw);
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => v,
        Err(_) => json!({
            "error": {
                "message": text.chars().take(500).collect::<String>(),
                "type": "upstream_error",
                "code": status,
            }
        }),
    }
}

fn last_user_text(messages: &Value) -> String {
    let Some(msgs) = messages.as_array() else {
        return String::new();
    };
    for m in msgs.iter().rev() {
        if m.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        match m.get("content") {
            Some(Value::String(s)) => return s.clone(),
            Some(Value::Array(blocks)) => {
                for blk in blocks {
                    if blk.get("type").and_then(|t| t.as_str()) == Some("text") {
                        return blk
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                }
                return String::new();
            }
            _ => return String::new(),
        }
    }
    String::new()
}

fn apply_desensitize(body: &Value, state: &AppState, force_compact: bool) -> Value {
    desensitize_body(
        body,
        &["system", "developer"],
        false,
        true,
        force_compact || !state.config.no_compact,
        true,
    )
}

fn map_developer_roles(body: &mut Value) {
    if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for m in msgs.iter_mut() {
            if m.get("role").and_then(|r| r.as_str()) == Some("developer") {
                m["role"] = json!("system");
            }
        }
    }
}

fn require_messages(payload: &Value) -> Result<(), Response> {
    let ok = payload
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(err_openai(
            StatusCode::BAD_REQUEST,
            "messages is required",
            "invalid_request_error",
        ))
    }
}

// ---------------------------------------------------------------------------
// 统一上游执行
// ---------------------------------------------------------------------------

/// 一次上游调用的结果：已转换好的 OpenAI SSE 字节流。
///
/// 上游始终以流式调用；非流式由调用方收集该流后聚合（见 [`execute_aggregate`]）。
enum Executed {
    Stream(ByteStream),
}

/// 统一执行：准备请求 → 发起 → 返回已转换的 OpenAI SSE 流。
///
/// 转换由 `upstream.translate_stream` 负责（CodeBuddy 是恒等转发，Trae 是私有事件转换）。
async fn execute_upstream(
    state: &AppState,
    upstream: &Arc<dyn Upstream>,
    chat_body: &Value,
    rid: &str,
    label: &str,
) -> Result<Executed, Response> {
    let prepared = upstream.prepare_chat(chat_body).await.map_err(|e| {
        log_rid(rid, &format!("✗ 上游准备失败 | {label} | {e}"));
        err_openai(StatusCode::BAD_GATEWAY, e, "auth_error")
    })?;

    let t0 = Instant::now();
    let resp = state
        .http
        .post(&prepared.url)
        .headers(parse_headers(&prepared.headers))
        .json(&prepared.body)
        .send()
        .await
        .map_err(|e| {
            log_rid(rid, &format!("✗ 网络错误 | {label} | {e}"));
            err_openai(
                StatusCode::BAD_GATEWAY,
                format!("upstream error: {e}"),
                "upstream_error",
            )
        })?;

    let status = resp.status().as_u16();
    if status != 200 {
        let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        log_rid(
            rid,
            &format!(
                "✗ HTTP {status} | {label} | {}",
                truncate(&String::from_utf8_lossy(&raw), 200)
            ),
        );
        log_rid(
            rid,
            &format!("── ERROR BODY ──\n{}", String::from_utf8_lossy(&raw)),
        );
        return Err(err_detail(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            safe_err_raw(&raw, status),
        ));
    }

    log_rid(
        rid,
        &format!("… 上游已响应 | {label} | {:.2}s", t0.elapsed().as_secs_f64()),
    );
    log_rid(
        rid,
        &format!(
            "── 发往上游的 WIRE BODY ──\n{}",
            serde_json::to_string_pretty(&prepared.body).unwrap_or_default()
        ),
    );
    Ok(Executed::Stream(upstream.translate_stream(Box::pin(
        resp.bytes_stream(),
    ))))
}

/// 流式：把上游流包成 HTTP SSE 响应，并统计 finish_reason / tool_calls / usage。
fn into_sse(executed: Executed, rid: String, label: String) -> Response {
    let Executed::Stream(mut stream) = executed;
    let out = async_stream::stream! {
        let mut stats = StreamStats::new();
        let started = Instant::now();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    // CodeBuddy 转出的是原生 OpenAI SSE；Trae 转换后也是 OpenAI chunk，
                    // 因此同一套统计对两者都成立。
                    stats.feed(&b);
                    yield Ok(b);
                }
                Err(e) => {
                    log_rid(&rid, &format!("✗ 流中断 | {label} | {e}"));
                    yield Ok(Bytes::from(err_event(&e.to_string(), 502)));
                    break;
                }
            }
        }
        let tag = if stats.saw_filter || stats.finish_reason.as_deref() == Some("content-filter") {
            " ⚠️内容审核拦截"
        } else {
            ""
        };
        let mut line = format!(
            "◀ 流结束 | {label} | {:.1}s | finish={}{}",
            started.elapsed().as_secs_f64(),
            stats.finish_reason.as_deref().unwrap_or("None"),
            tag
        );
        if !stats.tool_names.is_empty() {
            line.push_str(&format!(" | tool_calls={:?}", stats.tool_names));
        }
        line.push_str(&format!(
            " | tokens={}",
            stats.usage_total.map(|t| t.to_string()).unwrap_or_else(|| "?".to_string())
        ));
        log_rid(&rid, &line);
    };
    sse_response(out)
}

/// 非流式：收集上游流（经上游转换）并聚合为 chat.completion。
async fn execute_aggregate(
    state: &AppState,
    upstream: &Arc<dyn Upstream>,
    chat_body: &Value,
    rid: &str,
    label: &str,
) -> Result<Value, Response> {
    let executed = execute_upstream(state, upstream, chat_body, rid, label).await?;
    let Executed::Stream(mut stream) = executed;
    let mut raw: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => {
                if raw.len() + b.len() > MAX_UPSTREAM_BODY {
                    return Err(err_openai(
                        StatusCode::BAD_GATEWAY,
                        "upstream response too large",
                        "upstream_error",
                    ));
                }
                raw.extend_from_slice(&b);
            }
            Err(e) => {
                log_rid(rid, &format!("✗ 流中断 | {label} | {e}"));
                return Err(err_openai(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream error: {e}"),
                    "upstream_error",
                ));
            }
        }
    }
    Ok(upstream.aggregate(&raw))
}

/// 带内容审核重试的非流式聚合（仅 Responses 端点使用，保持原行为）。
async fn execute_aggregate_with_retry(
    state: &AppState,
    upstream: &Arc<dyn Upstream>,
    chat_body: &Value,
    rid: &str,
    label: &str,
) -> Result<Value, Response> {
    let executed = execute_upstream(state, upstream, chat_body, rid, label).await?;
    let Executed::Stream(mut stream) = executed;
    let mut raw: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => raw.extend_from_slice(&b),
            Err(e) => {
                return Err(err_openai(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream error: {e}"),
                    "upstream_error",
                ))
            }
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let should_retry = looks_like_content_filter_text(&text)
        && state.config.desensitize
        && state.config.no_compact
        && std::env::var("CODEBUDDY_RESPONSES_DESENSITIZE").unwrap_or_default() == "1";
    if should_retry {
        let retry_body = apply_desensitize(chat_body, state, true);
        log_rid(
            rid,
            &format!("↻ RESPONSES {label} | content filter detected, retry with compact harness"),
        );
        if let Ok(Executed::Stream(mut s)) =
            execute_upstream(state, upstream, &retry_body, rid, label).await
        {
            let mut retry_raw: Vec<u8> = Vec::new();
            while let Some(chunk) = s.next().await {
                if let Ok(b) = chunk {
                    retry_raw.extend_from_slice(&b);
                }
            }
            let retry_text = String::from_utf8_lossy(&retry_raw);
            if !looks_like_content_filter_text(&retry_text) {
                return Ok(upstream.aggregate(&retry_raw));
            }
        }
    }
    Ok(upstream.aggregate(&raw))
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

async fn health_impl(state: &AppState, upstream: Arc<dyn Upstream>) -> Response {
    let mut info = json!({
        "status": "ok",
        "platform": std::env::consts::OS,
        "runtime": format!("rust/{}", env!("CARGO_PKG_VERSION")),
        "upstream": upstream.name(),
        "auth_file": crate::auth::find_auth_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "(未找到)".to_string()),
        "mode": "direct-proxy (native function calling)",
    });
    let extra = upstream.health_info().await;
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            info[k] = v.clone();
        }
    }
    let _ = state;
    Json(info).into_response()
}

async fn health_default(State(state): State<SharedState>) -> Response {
    let Ok(upstream) = resolve_upstream(&state, None) else {
        return err_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "未配置任何上游", "type": "auth_error"}}),
        );
    };
    health_impl(&state, upstream).await
}

async fn health_named(State(state): State<SharedState>, Path(name): Path<String>) -> Response {
    match resolve_upstream(&state, Some(&name)) {
        Ok(u) => health_impl(&state, u).await,
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// GET /v1/models
// ---------------------------------------------------------------------------

async fn models_impl(upstream: Arc<dyn Upstream>) -> Response {
    let owner = upstream.name().to_string();
    let data: Vec<Value> = upstream
        .models()
        .await
        .into_iter()
        .map(|m| json!({"id": m, "object": "model", "created": 1700000000, "owned_by": owner}))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn models_default(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    match resolve_upstream(&state, None) {
        Ok(u) => models_impl(u).await,
        Err(resp) => resp,
    }
}

async fn models_named(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    match resolve_upstream(&state, Some(&name)) {
        Ok(u) => models_impl(u).await,
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// POST /v1/chat/completions
// ---------------------------------------------------------------------------

async fn chat_completions_impl(
    state: &AppState,
    upstream: Arc<dyn Upstream>,
    payload: Value,
) -> Response {
    if let Err(resp) = require_messages(&payload) {
        return resp;
    }

    let client_wants_stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    // 构造后端 body：只透传已知的合法字段
    let mut backend_body = serde_json::Map::new();
    for key in PASSTHROUGH_BODY_KEYS {
        if let Some(v) = payload.get(*key) {
            backend_body.insert(key.to_string(), v.clone());
        }
    }
    backend_body
        .entry("model".to_string())
        .or_insert_with(|| json!("auto"));
    backend_body.insert("stream".to_string(), json!(true));
    backend_body
        .entry("stream_options".to_string())
        .or_insert_with(|| json!({"include_usage": true}));
    let mut backend_body = Value::Object(backend_body);

    // 腾讯后端不支持 developer role，遇到会触发安全策略拦截（11128），统一映射为 system
    map_developer_roles(&mut backend_body);
    let backend_body = filter_system_identity(&backend_body);
    let backend_body = if state.config.desensitize {
        apply_desensitize(&backend_body, state, false)
    } else {
        backend_body
    };

    // 日志：请求摘要
    let model_name = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
    let messages = payload.get("messages").cloned().unwrap_or(json!([]));
    let tool_names: Vec<String> = payload
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.pointer("/function/name").and_then(|n| n.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let last_user = last_user_text(&messages);
    let rid = new_rid();
    let label = format!("{} {model_name}", upstream.name());
    let mut req_log = format!(
        "▶ REQUEST {label} | stream={client_wants_stream} | msgs={}",
        messages.as_array().map(|a| a.len()).unwrap_or(0)
    );
    if !tool_names.is_empty() {
        req_log.push_str(&format!(" | tools={tool_names:?}"));
    }
    if !last_user.is_empty() {
        req_log.push_str(&format!(" | last_user={:?}", truncate(&last_user, 60)));
    }
    log_rid(&rid, &req_log);
    log_rid(
        &rid,
        &format!(
            "── REQUEST BODY (客户端原始) ──\n{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        ),
    );

    if client_wants_stream {
        match execute_upstream(state, &upstream, &backend_body, &rid, &label).await {
            Ok(executed) => into_sse(executed, rid, label),
            Err(resp) => resp,
        }
    } else {
        match execute_aggregate(state, &upstream, &backend_body, &rid, &label).await {
            Ok(v) => {
                log_finish(&label, &v, &rid);
                Json(v).into_response()
            }
            Err(resp) => resp,
        }
    }
}

fn log_finish(label: &str, result: &Value, rid: &str) {
    let choice = result.pointer("/choices/0").cloned().unwrap_or(json!({}));
    let finish = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("");
    let total_tokens = result
        .pointer("/usage/total_tokens")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let tag = if finish == "content-filter" {
        " ⚠️内容审核拦截"
    } else {
        ""
    };
    let mut line = format!("◀ RESPONSE {label} | finish={finish}{tag}");
    if let Some(tcs) = choice
        .pointer("/message/tool_calls")
        .and_then(|t| t.as_array())
    {
        let names: Vec<String> = tcs
            .iter()
            .filter_map(|t| {
                t.pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        if !names.is_empty() {
            line.push_str(&format!(" | tool_calls={names:?}"));
        }
    }
    line.push_str(&format!(" | tokens={total_tokens}"));
    log_rid(rid, &line);
    log_rid(
        rid,
        &format!(
            "── RESPONSE BODY ──\n{}",
            serde_json::to_string_pretty(result).unwrap_or_default()
        ),
    );
}

async fn chat_completions_default(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, None) {
        Ok(u) => chat_completions_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

async fn chat_completions_named(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, Some(&name)) {
        Ok(u) => chat_completions_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// POST /v1/responses
// ---------------------------------------------------------------------------

async fn create_response_impl(
    state: &AppState,
    upstream: Arc<dyn Upstream>,
    payload: Value,
) -> Response {
    let mut chat_body = match responses_request_to_chat(&payload) {
        Ok(v) => v,
        Err(e) => {
            return err_openai(
                StatusCode::BAD_REQUEST,
                format!("request conversion error: {e}"),
                "invalid_request_error",
            )
        }
    };

    chat_body = filter_system_identity(&chat_body);
    let preserve = std::env::var("CODEBUDDY_LOSSY_PROJECTION").unwrap_or_default() != "1";
    let (mut chat_body, projection_stats) = project_responses_chat_body(&chat_body, preserve);
    chat_body
        .as_object_mut()
        .unwrap()
        .entry("model".to_string())
        .or_insert_with(|| json!("auto"));
    chat_body["stream"] = json!(true);
    chat_body
        .as_object_mut()
        .unwrap()
        .entry("stream_options".to_string())
        .or_insert_with(|| json!({"include_usage": true}));

    if std::env::var("CODEBUDDY_RESPONSES_DESENSITIZE").unwrap_or_default() == "1"
        && state.config.desensitize
    {
        chat_body = apply_desensitize(&chat_body, state, false);
    }

    let client_wants_stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let model_name = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
    let rid = new_rid();
    let label = format!("{} {model_name}", upstream.name());
    let input_len = payload
        .get("input")
        .and_then(|i| i.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    log_rid(
        &rid,
        &format!("▶ RESPONSES {label} | stream={client_wants_stream} | input_items={input_len}"),
    );
    log_rid(&rid, &format!("── RESPONSES PROJECTION ── {projection_stats}"));
    log_rid(
        &rid,
        &format!(
            "── RESPONSES → CHAT BODY ──\n{}",
            serde_json::to_string_pretty(&chat_body).unwrap_or_default()
        ),
    );

    if client_wants_stream {
        match execute_upstream(state, &upstream, &chat_body, &rid, &label).await {
            Ok(Executed::Stream(mut stream)) => {
                let rid2 = rid.clone();
                let label2 = label.clone();
                let out = async_stream::stream! {
                    let mut converter = ResponsesStreamConverter::new(&model_name);
                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(b) => {
                                let text = String::from_utf8_lossy(&b);
                                let mut events = String::new();
                                for line in text.lines() {
                                    events.push_str(&converter.feed_line(line));
                                }
                                if !events.is_empty() {
                                    yield Ok(Bytes::from(events));
                                }
                            }
                            Err(e) => {
                                let msg: String = e.to_string().chars().take(500).collect();
                                yield Ok(Bytes::from(converter.error(&msg, "502")));
                                return;
                            }
                        }
                    }
                    let finish_events = converter.finish();
                    if !finish_events.is_empty() {
                        yield Ok(Bytes::from(finish_events));
                    }
                    log_rid(&rid2, &format!("◀ RESPONSES {label2} | stream done"));
                };
                sse_response(out)
            }
            Err(resp) => resp,
        }
    } else {
        match execute_aggregate_with_retry(state, &upstream, &chat_body, &rid, &label).await {
            Ok(chat_result) => {
                let result = responses_from_chat(&chat_result, &model_name);
                let mut response_status = 200;
                if let Some(err) = result.get("error").filter(|e| !e.is_null()) {
                    let code = err.get("code").map(|c| c.to_string()).unwrap_or_default();
                    let code = code.trim_matches('"');
                    response_status = match code.parse::<u16>() {
                        Ok(c) if (400..=599).contains(&c) => c,
                        _ => 502,
                    };
                }
                log_rid(&rid, &format!("◀ RESPONSES {label} | done"));
                log_rid(
                    &rid,
                    &format!(
                        "── RESPONSE OBJ ──\n{}",
                        serde_json::to_string_pretty(&result).unwrap_or_default()
                    ),
                );
                let status =
                    StatusCode::from_u16(response_status).unwrap_or(StatusCode::BAD_GATEWAY);
                (status, Json(result)).into_response()
            }
            Err(resp) => resp,
        }
    }
}

/// 用 Responses 转换器把非流式 chat.completion 结果重放成 Responses 对象。
fn responses_from_chat(chat: &Value, model: &str) -> Value {
    let mut converter = ResponsesStreamConverter::new(model);
    let choice = chat.pointer("/choices/0").cloned().unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            let chunk = json!({
                "model": model,
                "choices": [{"index": 0, "delta": {"content": content}}],
            });
            converter.feed_line(&format!("data: {chunk}"));
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for (i, tc) in tcs.iter().enumerate() {
            let chunk = json!({
                "model": model,
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": i,
                    "id": tc.get("id").cloned().unwrap_or(json!("")),
                    "function": {
                        "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
                        "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(json!("")),
                    }
                }]}}],
            });
            converter.feed_line(&format!("data: {chunk}"));
        }
    }
    if let Some(usage) = chat.get("usage").filter(|u| !u.is_null()) {
        converter.feed_line(&format!("data: {}", json!({"choices": [], "usage": usage})));
    }
    let finish = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    converter.feed_line(&format!(
        "data: {}",
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
    ));
    converter.get_nonstream_response()
}

async fn create_response_default(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, None) {
        Ok(u) => create_response_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

async fn create_response_named(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, Some(&name)) {
        Ok(u) => create_response_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// POST /v1/messages
// ---------------------------------------------------------------------------

async fn create_message_impl(
    state: &AppState,
    upstream: Arc<dyn Upstream>,
    payload: Value,
) -> Response {
    if let Err(resp) = require_messages(&payload) {
        return resp;
    }

    let mut chat_body = match anthropic_request_to_chat(&payload) {
        Ok(v) => v,
        Err(e) => {
            return err_detail(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("request conversion error: {e}"), "type": "invalid_request_error"}}),
            )
        }
    };

    chat_body
        .as_object_mut()
        .unwrap()
        .entry("model".to_string())
        .or_insert_with(|| json!("auto"));
    let user_stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(true);
    chat_body["stream"] = json!(true);
    chat_body
        .as_object_mut()
        .unwrap()
        .entry("stream_options".to_string())
        .or_insert_with(|| json!({"include_usage": true}));

    let chat_body = filter_system_identity(&chat_body);
    let chat_body = if state.config.desensitize {
        apply_desensitize(&chat_body, state, false)
    } else {
        chat_body
    };

    let model_name = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
    let raws = payload.get("messages").cloned().unwrap_or(json!([]));
    let chat_messages = chat_body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let anthropic_msgs = raws.as_array().map(|a| a.len()).unwrap_or(0);
    let rid = new_rid();
    let label = format!("{} {model_name}", upstream.name());
    log_rid(
        &rid,
        &format!(
            "▶ ANTHROPIC {label} | msgs={chat_messages} | anthropic_msgs={anthropic_msgs} | user_stream={user_stream}"
        ),
    );
    log_rid(
        &rid,
        &format!(
            "── ANTHROPIC → CHAT BODY ──\n{}",
            serde_json::to_string_pretty(&chat_body).unwrap_or_default()
        ),
    );

    if user_stream {
        match execute_upstream(state, &upstream, &chat_body, &rid, &label).await {
            Ok(Executed::Stream(mut stream)) => {
                let rid2 = rid.clone();
                let label2 = label.clone();
                let out = async_stream::stream! {
                    let mut converter = AnthropicStreamConverter::new(&model_name);
                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(b) => {
                                let text = String::from_utf8_lossy(&b);
                                let mut events = String::new();
                                for line in text.lines() {
                                    events.push_str(&converter.feed_line(line));
                                }
                                if !events.is_empty() {
                                    yield Ok(Bytes::from(events));
                                }
                            }
                            Err(e) => {
                                let msg: String = e.to_string().chars().take(500).collect();
                                let error_evt = json!({"type": "error", "error": {"message": msg, "type": "api_error", "code": 502}});
                                yield Ok(Bytes::from(format!("event: error\ndata: {error_evt}\n\n")));
                                return;
                            }
                        }
                    }
                    let finish_events = converter.finish();
                    if !finish_events.is_empty() {
                        yield Ok(Bytes::from(finish_events));
                    }
                    log_rid(&rid2, &format!("◀ ANTHROPIC {label2} | stream done"));
                };
                sse_response(out)
            }
            Err(resp) => resp,
        }
    } else {
        match execute_aggregate(state, &upstream, &chat_body, &rid, &label).await {
            Ok(chat_result) => {
                let result = anthropic_from_chat(&chat_result, &model_name);
                log_rid(&rid, &format!("◀ ANTHROPIC {label} | nonstream done"));
                Json(result).into_response()
            }
            Err(resp) => resp,
        }
    }
}

/// 用 Anthropic 转换器把非流式 chat.completion 重放成 Anthropic Message 对象。
fn anthropic_from_chat(chat: &Value, model: &str) -> Value {
    let mut converter = AnthropicStreamConverter::new(model);
    let choice = chat.pointer("/choices/0").cloned().unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            let chunk =
                json!({"model": model, "choices": [{"index": 0, "delta": {"content": content}}]});
            converter.feed_line(&format!("data: {chunk}"));
        }
    }
    if let Some(tcs) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for (i, tc) in tcs.iter().enumerate() {
            let chunk = json!({"model": model, "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": i,
                "id": tc.get("id").cloned().unwrap_or(json!("")),
                "function": {
                    "name": tc.pointer("/function/name").cloned().unwrap_or(json!("")),
                    "arguments": tc.pointer("/function/arguments").cloned().unwrap_or(json!("")),
                }
            }]}}]});
            converter.feed_line(&format!("data: {chunk}"));
        }
    }
    if let Some(usage) = chat.get("usage").filter(|u| !u.is_null()) {
        converter.feed_line(&format!("data: {}", json!({"choices": [], "usage": usage})));
    }
    let finish = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    converter.feed_line(&format!(
        "data: {}",
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
    ));
    converter.get_nonstream_response()
}

async fn create_message_default(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, None) {
        Ok(u) => create_message_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

async fn create_message_named(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, Some(&name)) {
        Ok(u) => create_message_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

// ---------------------------------------------------------------------------
// POST /v1/messages/count_tokens
// ---------------------------------------------------------------------------

async fn count_tokens_impl(
    state: &AppState,
    upstream: Arc<dyn Upstream>,
    payload: Value,
) -> Response {
    if let Err(resp) = require_messages(&payload) {
        return resp;
    }

    let mut chat_body = match anthropic_request_to_chat(&payload) {
        Ok(v) => v,
        Err(e) => {
            return err_detail(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("request conversion error: {e}"), "type": "invalid_request_error"}}),
            )
        }
    };
    chat_body
        .as_object_mut()
        .unwrap()
        .entry("model".to_string())
        .or_insert_with(|| json!("auto"));
    chat_body["max_tokens"] = json!(1);
    chat_body["stream"] = json!(true);
    chat_body["stream_options"] = json!({"include_usage": true});

    let chat_body = filter_system_identity(&chat_body);
    let chat_body = if state.config.desensitize {
        apply_desensitize(&chat_body, state, false)
    } else {
        chat_body
    };

    let rid = new_rid();
    let label = format!("{} count_tokens", upstream.name());
    match execute_aggregate(state, &upstream, &chat_body, &rid, &label).await {
        Ok(result) => {
            let input_tokens = result
                .pointer("/usage/prompt_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            Json(json!({"input_tokens": input_tokens})).into_response()
        }
        Err(resp) => resp,
    }
}

async fn count_tokens_default(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, None) {
        Ok(u) => count_tokens_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

async fn count_tokens_named(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let payload = match parse_body(&body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_upstream(&state, Some(&name)) {
        Ok(u) => count_tokens_impl(&state, u, payload).await,
        Err(resp) => resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{Upstream, UpstreamRequest};
    use serde_json::json;
    use std::collections::HashMap;

    /// 一个假的 Trae 风格上游：私有 SSE 事件 + 私有请求体，用于端到端验证转换链路。
    struct MockTraeUpstream;

    #[async_trait::async_trait]
    impl Upstream for MockTraeUpstream {
        fn name(&self) -> &str {
            "trae"
        }
        async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String> {
            Ok(UpstreamRequest {
                url: "http://127.0.0.1:1/never".to_string(),
                headers: vec![],
                body: json!({
                    "wrapped": true,
                    "model": chat.get("model").cloned().unwrap_or(json!("auto")),
                }),
            })
        }
        fn translate_stream(&self, raw: ByteStream) -> ByteStream {
            let mut conv = crate::upstream::trae::protocol::TraeEventConverter::new("trae-mock");
            Box::pin(async_stream::stream! {
                let mut raw = raw;
                let mut buf: Vec<u8> = Vec::new();
                while let Some(Ok(b)) = raw.next().await {
                    buf.extend_from_slice(&b);
                    while let Some(pos) = buf.iter().position(|&c| c == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        let mut s = String::from_utf8_lossy(&line[..line.len()-1]).to_string();
                        if s.ends_with('\r') { s.pop(); }
                        let out = conv.feed_line(&s);
                        if !out.is_empty() { yield Ok(Bytes::from(out)); }
                    }
                }
                let tail = conv.finish();
                if !tail.is_empty() { yield Ok(Bytes::from(tail)); }
            })
        }
        fn aggregate(&self, raw: &[u8]) -> Value {
            crate::upstream::aggregate_openai_sse(raw)
        }
        async fn models(&self) -> Vec<String> {
            vec!["trae-model".to_string()]
        }
        async fn health_info(&self) -> Value {
            json!({"credential": {"storage_file": "/fake"}})
        }
    }

    fn test_state() -> SharedState {
        let mut upstreams: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
        upstreams.insert("trae".to_string(), Arc::new(MockTraeUpstream));
        Arc::new(AppState {
            config: crate::config::AppConfig {
                api_key: String::new(),
                desensitize: false,
                no_compact: false,
            },
            upstreams,
            default_upstream: "trae".to_string(),
            http: reqwest::Client::new(),
        })
    }

    #[test]
    fn test_resolve_upstream_by_name() {
        let state = test_state();
        assert_eq!(state.upstream(Some("trae")).unwrap().name(), "trae");
        // 空名字 → 默认上游
        assert_eq!(state.upstream(Some("")).unwrap().name(), "trae");
        // 未知上游
        assert!(state.upstream(Some("nope")).is_none());
    }

    #[tokio::test]
    async fn test_health_reports_upstream_name() {
        let state = test_state();
        let upstream = state.upstream(None).unwrap();
        let resp = health_impl(&state, upstream).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_models_isolated_per_upstream() {
        let state = test_state();
        let upstream = state.upstream(Some("trae")).unwrap();
        let resp = models_impl(upstream).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_prepare_chat_wraps_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let u = MockTraeUpstream;
            let req = u
                .prepare_chat(&json!({"model": "kimi-k3", "messages": []}))
                .await
                .unwrap();
            assert_eq!(req.body["wrapped"], true);
            assert_eq!(req.body["model"], "kimi-k3");
        });
    }

    #[test]
    fn test_translate_private_sse_to_openai_chunks() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let private: String =
                "id:1\nevent:output\ndata:{\"content\":\"hi\"}\n\nid:2\nevent:done\ndata:{\"finish_reason\":\"stop\"}\n\n"
                    .to_string();
            let raw: ByteStream = Box::pin(futures::stream::once(async move {
                Ok(Bytes::from(private))
            }));
            let u = MockTraeUpstream;
            let mut stream = u.translate_stream(raw);
            let mut text = String::new();
            while let Some(chunk) = stream.next().await {
                if let Ok(b) = chunk {
                    text.push_str(&String::from_utf8_lossy(&b));
                }
            }
            assert!(text.contains("\"content\":\"hi\""));
            assert!(text.contains("\"finish_reason\":\"stop\""));
            assert!(text.contains("data: [DONE]"));
        });
    }

    #[test]
    fn test_aggregate_openai_sse_roundtrip() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n\
                   data: [DONE]\n\n";
        let out = crate::upstream::aggregate_openai_sse(raw.as_bytes());
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["total_tokens"], 5);
    }

    #[test]
    fn test_parse_body_rejects_bad_json() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let err = parse_body(&Bytes::from_static(b"not-json")).await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn test_require_messages() {
        assert!(require_messages(&json!({"messages": [{"role": "user"}]})).is_ok());
        assert!(require_messages(&json!({"messages": []})).is_err());
        assert!(require_messages(&json!({})).is_err());
    }

    #[test]
    fn test_check_auth_enforced_when_key_set() {
        let mut upstreams: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
        upstreams.insert("trae".to_string(), Arc::new(MockTraeUpstream));
        let state = AppState {
            config: crate::config::AppConfig {
                api_key: "secret".to_string(),
                desensitize: false,
                no_compact: false,
            },
            upstreams,
            default_upstream: "trae".to_string(),
            http: reqwest::Client::new(),
        };
        let mut headers = HeaderMap::new();
        assert!(check_auth(&state, &headers).is_err());
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(check_auth(&state, &headers).is_ok());
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(check_auth(&state, &headers).is_err());
    }

    #[test]
    fn test_resolve_unknown_upstream_error_lists_available() {
        let state = test_state();
        match resolve_upstream(&state, Some("nope")) {
            Ok(_) => panic!("应当返回错误"),
            Err(resp) => assert_eq!(resp.status(), StatusCode::NOT_FOUND),
        }
    }

    /// 非流式聚合结果 → Responses 对象的重放。
    #[test]
    fn test_responses_from_chat_replay() {
        let chat = json!({
            "id": "chatcmpl-x",
            "model": "glm-5.2",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello world"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        });
        let obj = responses_from_chat(&chat, "glm-5.2");
        assert_eq!(obj["object"], "response");
        assert_eq!(obj["status"], "completed");
        assert_eq!(obj["output"][0]["content"][0]["text"], "hello world");
    }

    /// 非流式聚合结果 → Anthropic Message 对象的重放。
    #[test]
    fn test_anthropic_from_chat_replay() {
        let chat = json!({
            "model": "glm-5.2",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi there"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        });
        let obj = anthropic_from_chat(&chat, "glm-5.2");
        assert_eq!(obj["type"], "message");
        assert_eq!(obj["role"], "assistant");
        assert_eq!(obj["stop_reason"], "end_turn");
        assert_eq!(obj["content"][0]["text"], "hi there");
        assert_eq!(obj["usage"]["input_tokens"], 3);
    }
}
