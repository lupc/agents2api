//! axum 路由与处理器：/health、/v1/models、/v1/chat/completions、
//! /v1/responses、/v1/messages、/v1/messages/count_tokens。

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::time::Instant;

use crate::anthropic::{anthropic_request_to_chat, AnthropicStreamConverter};
use crate::auth::{parse_headers, BACKEND};
use crate::config::{log, log_rid, new_rid, AppState, SharedState};
use crate::desensitize::desensitize_body;
use crate::identity::filter_system_identity;
use crate::models::get_available_models;
use crate::projection::project_responses_chat_body;
use crate::responses_adapter::{responses_request_to_chat, ResponsesStreamConverter};
use crate::upstream::{
    err_event, line_stream, looks_like_content_filter_text, parse_data_line, ChatAggregator,
    StreamStats,
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

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(create_response))
        .route("/v1/messages", post(create_message))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 复刻 FastAPI HTTPException 的响应格式：{"detail": <detail>}。
fn err_detail(status: StatusCode, detail: Value) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn sse_response(stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static) -> Response {
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
    if let Some(auth) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
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

async fn backend_headers(state: &AppState) -> Result<Vec<(String, String)>, Response> {
    let Some(cred) = &state.cred else {
        return Err(err_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "error": {
                    "message": "未找到登录凭据，请先在桌面端登录 CodeBuddy/WorkBuddy",
                    "type": "auth_error",
                }
            }),
        ));
    };
    cred.get_headers().await.map_err(|e| {
        err_detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": {"message": e, "type": "auth_error"}}),
        )
    })
}

async fn parse_body(body: &Bytes) -> Result<Value, Response> {
    serde_json::from_slice::<Value>(body).map_err(|e| {
        err_detail(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": format!("bad json: {e}"), "type": "invalid_request_error"}}),
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

fn log_finish(model_name: &str, t0: Instant, result: &Value, rid: &str) {
    let elapsed = t0.elapsed().as_secs_f64();
    let choice = result
        .pointer("/choices/0")
        .cloned()
        .unwrap_or(json!({}));
    let finish = choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or("");
    let tcs = choice.pointer("/message/tool_calls").and_then(|t| t.as_array());
    let total_tokens = result
        .pointer("/usage/total_tokens")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "?".to_string());
    let tag = if finish == "content-filter" {
        " ⚠️内容审核拦截"
    } else {
        ""
    };
    let mut line = format!(
        "◀ RESPONSE {model_name} | {elapsed:.1}s | finish={finish}{tag}"
    );
    if let Some(tcs) = tcs {
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
        &format!("── RESPONSE BODY ──\n{}", serde_json::to_string_pretty(result).unwrap_or_default()),
    );
}

fn apply_desensitize(body: &Value, config: &AppState, force_compact: bool) -> Value {
    desensitize_body(
        body,
        &["system", "developer"],
        false,
        true,
        force_compact || !config.config.no_compact,
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

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

async fn health(State(state): State<SharedState>) -> Response {
    let mut info = json!({
        "status": "ok",
        "platform": std::env::consts::OS,
        "runtime": format!("rust/{}", env!("CARGO_PKG_VERSION")),
        "auth_file": crate::auth::find_auth_file()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "(未找到)".to_string()),
        "mode": "direct-proxy (native function calling)",
    });
    if let Some(cred) = &state.cred {
        match cred.summary().await {
            Ok(s) => info["credential"] = s,
            Err(e) => info["credential_error"] = json!(e),
        }
    }
    Json(info).into_response()
}

// ---------------------------------------------------------------------------
// GET /v1/models
// ---------------------------------------------------------------------------

async fn list_models(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_auth(&state, &headers) {
        return resp;
    }
    let data: Vec<Value> = get_available_models()
        .into_iter()
        .map(|m| {
            json!({"id": m, "object": "model", "created": 1700000000, "owned_by": "codebuddy"})
        })
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

// ---------------------------------------------------------------------------
// POST /v1/chat/completions
// ---------------------------------------------------------------------------

async fn chat_completions(
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

    let messages = payload.get("messages").cloned().unwrap_or(json!([]));
    if messages.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return err_detail(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "messages is required", "type": "invalid_request_error"}}),
        );
    }

    // 构造后端 body：只透传已知的合法字段
    let client_wants_stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let mut backend_body = serde_json::Map::new();
    for key in PASSTHROUGH_BODY_KEYS {
        if let Some(v) = payload.get(*key) {
            backend_body.insert(key.to_string(), v.clone());
        }
    }
    backend_body
        .entry("model".to_string())
        .or_insert_with(|| json!("auto"));
    // 后端只支持流式：始终以 stream=True 调后端，非流式由转换器聚合
    backend_body.insert("stream".to_string(), json!(true));
    backend_body
        .entry("stream_options".to_string())
        .or_insert_with(|| json!({"include_usage": true}));
    let mut backend_body = Value::Object(backend_body);

    // 腾讯后端不支持 developer role，遇到会触发安全策略拦截（11128），统一映射为 system
    map_developer_roles(&mut backend_body);
    let backend_body = filter_system_identity(&backend_body);

    // 可选：脱敏。缓解客户端合规模板被后端误判为敏感词。
    let backend_body = if state.config.desensitize {
        apply_desensitize(&backend_body, &state, false)
    } else {
        backend_body
    };

    // 日志：请求摘要
    let model_name = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
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
    let mut req_log = format!(
        "▶ REQUEST {model_name} | stream={client_wants_stream} | msgs={}",
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
            "── REQUEST BODY (发往后端) ──\n{}",
            serde_json::to_string_pretty(&backend_body).unwrap_or_default()
        ),
    );

    let upstream_headers = match backend_headers(&state).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let url = format!("{BACKEND}/v2/chat/completions");
    let t0 = Instant::now();

    if client_wants_stream {
        let rid2 = rid.clone();
        let model2 = model_name.clone();
        let stream = async_stream::stream! {
            let mut stats = StreamStats::new();
            'run: {
                let result = state
                    .http
                    .post(&url)
                    .headers(parse_headers(&upstream_headers))
                    .json(&backend_body)
                    .send()
                    .await;
                match result {
                    Err(e) => {
                        log_rid(&rid2, &format!("✗ 网络错误 | {model2} | {e}"));
                        yield Ok(Bytes::from(err_event(&e.to_string(), 502)));
                        break 'run;
                    }
                    Ok(resp) => {
                        if resp.status().as_u16() != 200 {
                            let status = resp.status().as_u16();
                            let raw = resp
                                .bytes()
                                .await
                                .map(|b| b.to_vec())
                                .unwrap_or_default();
                            let text = String::from_utf8_lossy(&raw).to_string();
                            log_rid(&rid2, &format!("✗ HTTP {status} | {model2} | {}", truncate(&text, 200)));
                            log_rid(&rid2, &format!("── ERROR BODY ──\n{text}"));
                            yield Ok(Bytes::from(err_event(&text, status)));
                            break 'run;
                        }
                        let mut bstream = resp.bytes_stream();
                        while let Some(chunk) = bstream.next().await {
                            match chunk {
                                Ok(b) => {
                                    stats.feed(&b);
                                    yield Ok(Bytes::from(b));
                                }
                                Err(e) => {
                                    log_rid(&rid2, &format!("✗ 网络错误 | {model2} | {e}"));
                                    yield Ok(Bytes::from(err_event(&e.to_string(), 502)));
                                    break 'run;
                                }
                            }
                        }
                    }
                }
            }
            // 流结束：输出完成日志
            let elapsed = t0.elapsed().as_secs_f64();
            let tag = if stats.saw_filter || stats.finish_reason.as_deref() == Some("content-filter") {
                " ⚠️内容审核拦截"
            } else {
                ""
            };
            let mut line = format!(
                "◀ RESPONSE {model2} | {elapsed:.1}s | stream finish={}{}",
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
            log_rid(&rid2, &line);
        };
        return sse_response(stream);
    }

    // 非流式：后端只支持流式，这里把后端 SSE 聚合成单个 chat.completion 响应
    let resp = state
        .http
        .post(&url)
        .headers(parse_headers(&upstream_headers))
        .json(&backend_body)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log_rid(&rid, &format!("✗ 网络错误 | {model_name} | {e}"));
            return err_detail(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": format!("upstream error: {e}"), "type": "upstream_error"}}),
            );
        }
    };
    if resp.status().as_u16() != 200 {
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        log_rid(
            &rid,
            &format!(
                "✗ HTTP {status} | {model_name} | {}",
                truncate(&String::from_utf8_lossy(&raw), 200)
            ),
        );
        log_rid(&rid, &format!("── ERROR BODY ──\n{}", String::from_utf8_lossy(&raw)));
        return err_detail(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            safe_err_raw(&raw, status),
        );
    }
    let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    let mut aggregator = ChatAggregator::new();
    for line in split_lines_lossy(&raw) {
        aggregator.feed_line(&line);
    }
    let collected = aggregator.finish();
    log_finish(&model_name, t0, &collected, &rid);
    Json(collected).into_response()
}

fn split_lines_lossy(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect()
}

// ---------------------------------------------------------------------------
// POST /v1/responses
// ---------------------------------------------------------------------------

async fn create_response(
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

    // 转换请求：Responses → Chat
    let mut chat_body = match responses_request_to_chat(&payload) {
        Ok(v) => v,
        Err(e) => {
            return err_detail(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("request conversion error: {e}"), "type": "invalid_request_error"}}),
            );
        }
    };

    chat_body = filter_system_identity(&chat_body);
    let preserve = std::env::var("CODEBUDDY_LOSSY_PROJECTION").unwrap_or_default() != "1";
    let (chat_body, projection_stats) = project_responses_chat_body(&chat_body, preserve);

    let mut chat_body = chat_body;
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
        chat_body = apply_desensitize(&chat_body, &state, false);
    }

    let client_wants_stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let model_name = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
    let rid = new_rid();
    let input_len = payload
        .get("input")
        .and_then(|i| i.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    log_rid(
        &rid,
        &format!("▶ RESPONSES {model_name} | stream={client_wants_stream} | input_items={input_len}"),
    );
    log_rid(&rid, &format!("── RESPONSES PROJECTION ── {projection_stats}"));
    log_rid(
        &rid,
        &format!(
            "── RESPONSES → CHAT BODY ──\n{}",
            serde_json::to_string_pretty(&chat_body).unwrap_or_default()
        ),
    );

    let upstream_headers = match backend_headers(&state).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let url = format!("{BACKEND}/v2/chat/completions");
    let t0 = Instant::now();

    if client_wants_stream {
        let rid2 = rid.clone();
        let model2 = model_name.clone();
        let stream = async_stream::stream! {
            let mut converter = ResponsesStreamConverter::new(&model2);
            let mut raw_tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
            {
                let result = state
                    .http
                    .post(&url)
                    .headers(parse_headers(&upstream_headers))
                    .json(&chat_body)
                    .send()
                    .await;
                match result {
                    Err(e) => {
                        log_rid(&rid2, &format!("✗ 网络错误 | {model2} | {e}"));
                        let msg: String = e.to_string().chars().take(500).collect();
                        yield Ok(Bytes::from(converter.error(&msg, "502")));
                        return;
                    }
                    Ok(resp) => {
                        if resp.status().as_u16() != 200 {
                            let status = resp.status().as_u16();
                            let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                            let text = String::from_utf8_lossy(&raw).to_string();
                            log_rid(&rid2, &format!("✗ HTTP {status} | {model2}"));
                            let msg: String = text.chars().take(500).collect();
                            yield Ok(Bytes::from(converter.error(&msg, &status.to_string())));
                            return;
                        }
                        let lines = line_stream(resp.bytes_stream());
                        let mut lines = std::pin::pin!(lines);
                        while let Some(line) = lines.next().await {
                            if !line.trim().is_empty() {
                                raw_tail.push_back(line.clone());
                                if raw_tail.len() > 30 {
                                    raw_tail.pop_front();
                                }
                            }
                            let events = converter.feed_line(&line);
                            if !events.is_empty() {
                                yield Ok(Bytes::from(events));
                            }
                        }
                    }
                }
            }
            // 发送收尾事件
            let finish_events = converter.finish();
            if !finish_events.is_empty() {
                yield Ok(Bytes::from(finish_events));
            }
            let elapsed = t0.elapsed().as_secs_f64();
            log_rid(&rid2, &format!("◀ RESPONSES {model2} | {elapsed:.1}s | stream done"));
            let tail: Vec<String> = raw_tail.into_iter().collect();
            log_rid(&rid2, &format!("── RESPONSES RAW SSE ──\n{}", tail.join("\n")));
        };
        return sse_response(stream);
    }

    // 非流式：聚合后端 SSE → 非流式 Response 对象
    let (status_code, raw, _final_body) =
        match post_backend_with_filter_retry(&state, &url, &upstream_headers, &chat_body, &rid, &model_name).await {
            Ok(v) => v,
            Err(resp) => return resp,
        };
    if status_code != 200 {
        log_rid(
            &rid,
            &format!(
                "✗ HTTP {status_code} | {model_name} | {}",
                truncate(&String::from_utf8_lossy(&raw), 200)
            ),
        );
        return err_detail(
            StatusCode::from_u16(status_code).unwrap_or(StatusCode::BAD_GATEWAY),
            safe_err_raw(&raw, status_code),
        );
    }
    let mut converter = ResponsesStreamConverter::new(&model_name);
    for line in split_lines_lossy(&raw) {
        converter.feed_line(&line);
    }
    let result = converter.get_nonstream_response();
    let mut response_status = 200;
    if let Some(err) = result.get("error").filter(|e| !e.is_null()) {
        let code = err.get("code").map(|c| c.to_string()).unwrap_or_default();
        let code = code.trim_matches('"');
        response_status = match code.parse::<u16>() {
            Ok(c) if (400..=599).contains(&c) => c,
            _ => 502,
        };
    }
    let elapsed = t0.elapsed().as_secs_f64();
    log_rid(&rid, &format!("◀ RESPONSES {model_name} | {elapsed:.1}s"));
    log_rid(
        &rid,
        &format!(
            "── RESPONSE OBJ ──\n{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        ),
    );
    let status = StatusCode::from_u16(response_status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status, Json(result)).into_response()
}

async fn post_backend_once(
    state: &AppState,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<(u16, Vec<u8>), String> {
    let resp = state
        .http
        .post(url)
        .headers(parse_headers(headers))
        .json(body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    Ok((status, raw))
}

async fn post_backend_with_filter_retry(
    state: &AppState,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    rid: &str,
    model_name: &str,
) -> Result<(u16, Vec<u8>, Value), Response> {
    let (status, raw) = post_backend_once(state, url, headers, body)
        .await
        .map_err(|e| {
            err_detail(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": format!("upstream error: {e}"), "type": "upstream_error"}}),
            )
        })?;
    let text = String::from_utf8_lossy(&raw).to_string();
    if status == 200
        && looks_like_content_filter_text(&text)
        && state.config.desensitize
        && state.config.no_compact
        && std::env::var("CODEBUDDY_RESPONSES_DESENSITIZE").unwrap_or_default() == "1"
    {
        let retry_body = apply_desensitize(body, state, true);
        log_rid(
            rid,
            &format!("↻ RESPONSES {model_name} | content filter detected, retry with compact harness"),
        );
        log_rid(
            rid,
            &format!(
                "── RESPONSES RETRY CHAT BODY ──\n{}",
                serde_json::to_string_pretty(&retry_body).unwrap_or_default()
            ),
        );
        let (retry_status, retry_raw) = post_backend_once(state, url, headers, &retry_body)
            .await
            .unwrap_or((status, raw.clone()));
        let retry_text = String::from_utf8_lossy(&retry_raw).to_string();
        if retry_status == 200 && !looks_like_content_filter_text(&retry_text) {
            return Ok((retry_status, retry_raw, retry_body));
        }
    }
    Ok((status, raw, body.clone()))
}

// ---------------------------------------------------------------------------
// POST /v1/messages
// ---------------------------------------------------------------------------

async fn create_message(
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

    let messages = payload.get("messages").cloned().unwrap_or(json!([]));
    if messages.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return err_detail(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "messages is required", "type": "invalid_request_error"}}),
        );
    }

    let mut chat_body = match anthropic_request_to_chat(&payload) {
        Ok(v) => v,
        Err(e) => {
            return err_detail(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("request conversion error: {e}"), "type": "invalid_request_error"}}),
            );
        }
    };

    chat_body
        .as_object_mut()
        .unwrap()
        .entry("model".to_string())
        .or_insert_with(|| json!("auto"));
    // 读取用户的 stream 参数；无论用户如何设置，都向后端请求流式响应
    let user_stream = payload.get("stream").and_then(|s| s.as_bool()).unwrap_or(true);
    chat_body["stream"] = json!(true);
    chat_body
        .as_object_mut()
        .unwrap()
        .entry("stream_options".to_string())
        .or_insert_with(|| json!({"include_usage": true}));

    let chat_body = filter_system_identity(&chat_body);
    let chat_body = if state.config.desensitize {
        apply_desensitize(&chat_body, &state, false)
    } else {
        chat_body
    };

    let model_name = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
    let chat_messages = chat_body.get("messages").and_then(|m| m.as_array()).map(|a| a.len()).unwrap_or(0);
    let anthropic_msgs = messages.as_array().map(|a| a.len()).unwrap_or(0);
    let rid = new_rid();
    log_rid(
        &rid,
        &format!(
            "▶ ANTHROPIC {model_name} | msgs={chat_messages} | anthropic_msgs={anthropic_msgs} | user_stream={user_stream}"
        ),
    );
    log_rid(
        &rid,
        &format!(
            "── ANTHROPIC → CHAT BODY ──\n{}",
            serde_json::to_string_pretty(&chat_body).unwrap_or_default()
        ),
    );

    let upstream_headers = match backend_headers(&state).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let url = format!("{BACKEND}/v2/chat/completions");
    let t0 = Instant::now();

    if user_stream {
        let rid2 = rid.clone();
        let model2 = model_name.clone();
        let stream = async_stream::stream! {
            let mut converter = AnthropicStreamConverter::new(&model2);
            {
                let result = state
                    .http
                    .post(&url)
                    .headers(parse_headers(&upstream_headers))
                    .json(&chat_body)
                    .send()
                    .await;
                match result {
                    Err(e) => {
                        log_rid(&rid2, &format!("✗ 网络错误 | {model2} | {e}"));
                        let error_evt = json!({"type": "error", "error": {"message": e.to_string().chars().take(500).collect::<String>(), "type": "api_error", "code": 502}});
                        yield Ok(Bytes::from(format!("event: error\ndata: {error_evt}\n\n")));
                        return;
                    }
                    Ok(resp) => {
                        if resp.status().as_u16() != 200 {
                            let status = resp.status().as_u16();
                            let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
                            let text = String::from_utf8_lossy(&raw).to_string();
                            log_rid(&rid2, &format!("✗ HTTP {status} | {model2} | {}", truncate(&text, 200)));
                            let error_evt = json!({"type": "error", "error": {"message": text.chars().take(500).collect::<String>(), "type": "api_error", "code": status}});
                            yield Ok(Bytes::from(format!("event: error\ndata: {error_evt}\n\n")));
                            return;
                        }
                        let lines = line_stream(resp.bytes_stream());
                        let mut lines = std::pin::pin!(lines);
                        while let Some(line) = lines.next().await {
                            let events = converter.feed_line(&line);
                            if !events.is_empty() {
                                yield Ok(Bytes::from(events));
                            }
                        }
                    }
                }
            }
            let finish_events = converter.finish();
            if !finish_events.is_empty() {
                yield Ok(Bytes::from(finish_events));
            }
            let elapsed = t0.elapsed().as_secs_f64();
            log_rid(&rid2, &format!("◀ ANTHROPIC {model2} | {elapsed:.1}s | stream done"));
        };
        return sse_response(stream);
    }

    // 非流式：收集完整响应并返回 JSON
    let resp = state
        .http
        .post(&url)
        .headers(parse_headers(&upstream_headers))
        .json(&chat_body)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log_rid(&rid, &format!("✗ 网络错误 | {model_name} | {e}"));
            return err_detail(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": e.to_string().chars().take(500).collect::<String>(), "type": "api_error", "code": 502}}),
            );
        }
    };
    if resp.status().as_u16() != 200 {
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        let text = String::from_utf8_lossy(&raw).to_string();
        log_rid(&rid, &format!("✗ HTTP {status} | {model_name} | {}", truncate(&text, 200)));
        return err_detail(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            json!({"error": {"message": text.chars().take(500).collect::<String>(), "type": "api_error", "code": status}}),
        );
    }
    let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    let mut converter = AnthropicStreamConverter::new(&model_name);
    for line in split_lines_lossy(&raw) {
        converter.feed_line(&line);
    }
    let result = converter.get_nonstream_response();
    let elapsed = t0.elapsed().as_secs_f64();
    log_rid(&rid, &format!("◀ ANTHROPIC {model_name} | {elapsed:.1}s | nonstream done"));
    Json(result).into_response()
}

// ---------------------------------------------------------------------------
// POST /v1/messages/count_tokens
// ---------------------------------------------------------------------------

async fn count_tokens(
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

    let messages = payload.get("messages").cloned().unwrap_or(json!([]));
    if messages.as_array().map(|a| a.is_empty()).unwrap_or(true) {
        return err_detail(
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "messages is required", "type": "invalid_request_error"}}),
        );
    }

    let mut chat_body = match anthropic_request_to_chat(&payload) {
        Ok(v) => v,
        Err(e) => {
            return err_detail(
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": format!("request conversion error: {e}"), "type": "invalid_request_error"}}),
            );
        }
    };

    // 最小化实际生成：只需要 usage 统计
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
        apply_desensitize(&chat_body, &state, false)
    } else {
        chat_body
    };

    let upstream_headers = match backend_headers(&state).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };
    let url = format!("{BACKEND}/v2/chat/completions");

    let resp = state
        .http
        .post(&url)
        .headers(parse_headers(&upstream_headers))
        .json(&chat_body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            log(&format!("✗ count_tokens network error: {e}"));
            return err_detail(
                StatusCode::BAD_GATEWAY,
                json!({"error": {"message": e.to_string().chars().take(500).collect::<String>(), "type": "api_error", "code": 502}}),
            );
        }
    };
    if resp.status().as_u16() != 200 {
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
        let text = String::from_utf8_lossy(&raw).to_string();
        log(&format!("✗ count_tokens HTTP {status}: {}", truncate(&text, 200)));
        return err_detail(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            json!({"error": {"message": text.chars().take(500).collect::<String>(), "type": "api_error", "code": status}}),
        );
    }

    // 解析 SSE 流，查找 usage 信息
    let mut input_tokens: i64 = 0;
    let lines = line_stream(resp.bytes_stream());
    let mut lines = std::pin::pin!(lines);
    while let Some(line) = lines.next().await {
        if line.trim().is_empty() || line.starts_with(':') {
            continue;
        }
        let Some(chunk) = parse_data_line(&line) else {
            continue;
        };
        // message_delta 事件直接包含 usage
        if let Some(usage) = chunk.get("usage") {
            let tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| usage.get("input_tokens").and_then(|v| v.as_i64()))
                .unwrap_or(0);
            if tokens > 0 {
                input_tokens = tokens;
            }
        } else if let Some(usage) = chunk.pointer("/message/usage") {
            let tokens = usage
                .get("prompt_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| usage.get("input_tokens").and_then(|v| v.as_i64()))
                .unwrap_or(0);
            if tokens > 0 {
                input_tokens = tokens;
            }
        }
    }
    Json(json!({"input_tokens": input_tokens})).into_response()
}
