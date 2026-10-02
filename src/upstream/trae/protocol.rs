//! Trae 私有协议：请求体构造与 SSE 事件 → OpenAI chunk 转换。
//!
//! 主通道是 IDE Agent 单请求端点 `POST {chatHost}/api/agent/v3/llm_utils_chat`：
//! - 请求体形如 `{messages, function, stream, config_name, model, ...}`；
//! - messages 的 `content` **必须是数组** `[{"type":"text","text":...}]`，传字符串会 400/4001；
//! - 响应**永远是 SSE**（即使 `stream:false`），且错误可能是 HTTP 200 + 内嵌 error 事件。
//!
//! 事件（`chunk.type`）：
//! - `output` / `text` → 文本增量，取 `content`（新）或 `response`（旧）
//! - `reasoning` → 思维链增量，取 `reasoning`（新）或 `reasoning_content`（旧）
//! - `tool_calls` → 工具调用数组，name 取 `name`/`function.name`/`tool_name`，
//!   args 取 `params`/`arguments`/`input`/`function.arguments`
//! - `token_usage` → usage（在 `data` 或顶层）
//! - `done` → 结束，`finish_reason` 默认 `stop`
//! - `error` → `{code, message}`

use serde_json::{json, Map, Value};

use super::auth::TraeAuthInfo;

pub const DEFAULT_CHAT_HOST: &str = "https://trae-api-cn.mchost.guru";
pub const DEFAULT_APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
pub const DEFAULT_IDE_VERSION: &str = "3.3.67";
pub const DEFAULT_FUNCTION: &str = "solo_work_lite";

/// 公开模型名 → Trae 内部名（可扩展；未命中则原样透传）。
const MODEL_ALIASES: &[(&str, &str)] = &[
    ("auto", "glm-5.2"),
    ("gpt-4o", "DeepSeek-V4-Pro"),
    ("gpt-4o-mini", "DeepSeek-V4-Flash"),
    ("deepseek-v3", "DeepSeek-V4-Pro"),
    ("deepseek-chat", "DeepSeek-V4-Pro"),
    ("claude-3-5-sonnet", "glm-5.2"),
    ("coding", "glm-5.1"),
];

/// 内置模型兜底列表（无法动态拉取时使用）。
pub const DEFAULT_MODELS: &[&str] = &[
    "auto",
    "glm-5.3",
    "glm-5.2",
    "glm-5-turbo",
    "glm-5",
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.6",
    "DeepSeek-V4-Pro",
    "DeepSeek-V4-Flash",
    "qwen3.8-max",
    "qwen-3.7-plus",
    "minimax-m3",
    "doubao-seed-2.1-pro",
];

/// 把公开模型名解析成 Trae 内部名。
pub fn resolve_model_name(model: &str) -> String {
    let lower = model.to_lowercase();
    for (public, internal) in MODEL_ALIASES {
        if lower == *public {
            return internal.to_string();
        }
    }
    model.to_string()
}

/// 把 OpenAI messages 转成 Trae 需要的形式：
/// - `content` 一律转成 `[{"type":"text","text":...}]`（Trae 硬性要求数组）
/// - `developer` → `system`
/// - 图像块转成 Trae 的 `{"type":"image","image_url":...}`（尽力而为，未知类型降级为文本）
pub fn normalize_messages(messages: &Value) -> Value {
    let Some(arr) = messages.as_array() else {
        return json!([]);
    };
    let mut out: Vec<Value> = Vec::new();
    for m in arr {
        if !m.is_object() {
            continue;
        }
        let role = match m.get("role").and_then(|r| r.as_str()).unwrap_or("user") {
            "developer" => "system",
            other => other,
        };
        let content = normalize_content(m.get("content").cloned().unwrap_or(Value::Null));
        let mut msg = Map::new();
        msg.insert("role".to_string(), json!(role));
        msg.insert("content".to_string(), content);
        // 保留 tool_calls / tool_call_id（若 Trae 侧支持）
        if let Some(tcs) = m.get("tool_calls") {
            msg.insert("tool_calls".to_string(), tcs.clone());
        }
        if let Some(id) = m.get("tool_call_id") {
            msg.insert("tool_call_id".to_string(), id.clone());
        }
        out.push(Value::Object(msg));
    }
    Value::Array(out)
}

fn normalize_content(content: Value) -> Value {
    match content {
        Value::String(s) => json!([{"type": "text", "text": s}]),
        Value::Array(blocks) => {
            let mut out: Vec<Value> = Vec::new();
            for b in &blocks {
                if let Value::String(s) = b {
                    out.push(json!({"type": "text", "text": s}));
                    continue;
                }
                let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" | "input_text" | "output_text" => {
                        out.push(json!({
                            "type": "text",
                            "text": b.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                        }));
                    }
                    "image_url" | "input_image" => {
                        let url = b
                            .pointer("/image_url/url")
                            .and_then(|u| u.as_str())
                            .or_else(|| b.get("image_url").and_then(|u| u.as_str()))
                            .unwrap_or("");
                        if !url.is_empty() {
                            out.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    // tool_result 等：把内容摊平成文本
                    _ => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            out.push(json!({"type": "text", "text": t}));
                        } else if let Some(c) = b.get("content") {
                            out.push(normalize_content(c.clone()));
                        }
                    }
                }
            }
            if out.is_empty() {
                json!([{"type": "text", "text": ""}])
            } else {
                Value::Array(out)
            }
        }
        Value::Null => json!([{"type": "text", "text": ""}]),
        other => json!([{"type": "text", "text": other.to_string()}]),
    }
}

/// 构造发往 Trae 的请求体（在标准 OpenAI Chat body 基础上包装）。
pub fn build_request_body(chat: &Value, function: &str) -> Value {
    let messages = normalize_messages(chat.get("messages").unwrap_or(&json!([])));
    let raw_model = chat
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto");
    let model = resolve_model_name(raw_model);

    let mut body = Map::new();
    body.insert("messages".to_string(), messages);
    body.insert("function".to_string(), json!(function));
    body.insert("stream".to_string(), json!(true));
    body.insert("config_name".to_string(), json!(model));
    body.insert("model".to_string(), json!(model));
    body.insert("request_id".to_string(), json!(crate::rand_hex(16)));
    body.insert("session_id".to_string(), json!(crate::rand_hex(16)));
    // 部分 Trae 部署要求 body 里带版本号（int64），缺失/类型错会返回 4001
    body.insert(
        "app_version_code".to_string(),
        json!(app_version_code_i64()),
    );
    body.insert("ide_version".to_string(), json!(ide_version()));

    // 透传常见生成参数
    for key in [
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "stop",
        "seed",
        "n",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
    ] {
        if let Some(v) = chat.get(key) {
            if !v.is_null() {
                body.insert(key.to_string(), v.clone());
            }
        }
    }
    Value::Object(body)
}

/// 构造发往 Trae 的请求头。
pub fn build_headers(info: &TraeAuthInfo, stream: bool) -> Vec<(String, String)> {
    let env_or = |key: &str, default: &str| -> String {
        std::env::var(key).unwrap_or_else(|_| default.to_string())
    };
    let app_id = env_or("TRAE_APP_ID", DEFAULT_APP_ID);
    let ide_version = env_or("TRAE_IDE_VERSION", DEFAULT_IDE_VERSION);
    let request_id = uuid_like();
    let trace_id = crate::rand_hex(16);
    let span_id: String = trace_id.chars().take(16).collect();

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Cloud-IDE-JWT {}", info.token),
        ),
        ("X-Cloudide-Token".to_string(), info.token.clone()),
        ("x-ide-token".to_string(), info.token.clone()),
        ("x-app-id".to_string(), app_id),
        ("x-uid".to_string(), info.user_id.clone()),
        ("x-device-id".to_string(), info.device_id.clone()),
        ("x-machine-id".to_string(), info.machine_id.clone()),
        ("x-request-id".to_string(), request_id.clone()),
        ("X-Request-ID".to_string(), request_id.clone()),
        ("X-Trae-Request-ID".to_string(), request_id),
        ("x-custom-trace-id".to_string(), trace_id.clone()),
        (
            "x-flow-traceparent".to_string(),
            format!("04-{trace_id}-{span_id}-01"),
        ),
        ("x-app-version".to_string(), "default".to_string()),
        ("x-app-version-code".to_string(), app_version_code()),
        ("x-ide-version".to_string(), ide_version),
        ("x-ide-version-code".to_string(), ide_version_code()),
        ("x-ide-version-type".to_string(), "stable".to_string()),
        ("x-device-type".to_string(), device_type().to_string()),
        ("x-os-version".to_string(), "10.0".to_string()),
        ("x-device-brand".to_string(), "unknown".to_string()),
        ("x-device-cpu".to_string(), std::env::consts::ARCH.to_string()),
        ("request-traffic-type".to_string(), "prod".to_string()),
        ("User-Agent".to_string(), "codebuddy2api-rs/2.0".to_string()),
    ];
    headers.push((
        "Accept".to_string(),
        if stream {
            "text/event-stream".to_string()
        } else {
            "application/json".to_string()
        },
    ));
    if info.host_variant.contains("SOLO") {
        headers.push(("x-trae-work".to_string(), "1".to_string()));
    }
    headers
}

fn uuid_like() -> String {
    format!(
        "{}-{}-{}-{}-{}",
        crate::rand_hex(4),
        crate::rand_hex(2),
        crate::rand_hex(2),
        crate::rand_hex(2),
        crate::rand_hex(6)
    )
}

/// `app_version_code` 是 int64（Trae 后端强类型校验）。
///
/// ⚠️ 该值需与 Trae 客户端版本匹配，否则上游返回 4001（param is invalid）。
/// 默认值来自本机 Trae CN 3.3.99 的探测；用 `TRAE_APP_VERSION_CODE` 可覆盖。
/// 若持续 4001，请从抓包中读取真实值后通过环境变量传入。
fn app_version_code_i64() -> i64 {
    std::env::var("TRAE_APP_VERSION_CODE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(202509150)
}

/// `x-app-version-code` 头：日期型版本号字符串。
fn app_version_code() -> String {
    std::env::var("TRAE_APP_VERSION_CODE").unwrap_or_else(|_| "20250915".to_string())
}

fn ide_version() -> String {
    std::env::var("TRAE_IDE_VERSION").unwrap_or_else(|_| DEFAULT_IDE_VERSION.to_string())
}

/// `x-ide-version-code`：同样按日期型版本号校验。
fn ide_version_code() -> String {
    std::env::var("TRAE_IDE_VERSION_CODE").unwrap_or_else(|_| "20250915".to_string())
}

fn device_type() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "mac",
        "linux" => "linux",
        other => other,
    }
}

/// Trae 错误码 → HTTP 状态码。
pub fn error_code_to_http(code: &str) -> u16 {
    match code {
        "1001" => 401,
        "1005" | "4008" => 402,
        "4010" => 403,
        "4017" => 403,
        "4001" | "4023" => 400,
        "4011" => 429,
        _ => 502,
    }
}

// ---------------------------------------------------------------------------
// SSE 事件 → OpenAI chunk
// ---------------------------------------------------------------------------

/// 把 Trae 私有 SSE 事件转成标准 OpenAI Chat chunk 的状态机。
///
/// 实测（Trae CN，2026-10）的上游 SSE 形状是：
/// ```text
/// id:1
/// event:error
/// data:{"code":4001,"message":"..."}
/// ```
/// 即**事件名在 `event:` 行**，`data:` 是纯 JSON 载荷（不含 type 字段）。
/// 兼容实现也接受 event 名来自载荷内的 `type`/`event`/`event_type` 字段。
pub struct TraeEventConverter {
    model: String,
    id: String,
    created: i64,
    started: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
    /// 已推送的 tool_calls 下标
    pending_event: Option<String>,
    pub emitted_output: bool,
    pub error: Option<(String, String)>, // (message, code)
}

impl TraeEventConverter {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            id: format!("chatcmpl-{}", crate::rand_hex(12)),
            created: chrono::Utc::now().timestamp(),
            started: false,
            finish_reason: None,
            usage: None,
            pending_event: None,
            emitted_output: false,
            error: None,
        }
    }

    /// 处理一行 Trae SSE，返回要发给客户端的 OpenAI SSE 文本。
    pub fn feed_line(&mut self, line: &str) -> String {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') {
            return String::new(); // 空行/注释（keepalive）
        }
        // `id:` 行忽略
        if let Some(rest) = line.strip_prefix("id:") {
            let _ = rest;
            return String::new();
        }
        // `event:` 行记录事件名，等随后的 data 行一起处理
        if let Some(rest) = line.strip_prefix("event:") {
            self.pending_event = Some(rest.trim().to_string());
            return String::new();
        }
        let Some(rest) = line.strip_prefix("data:") else {
            return String::new();
        };
        let rest = rest.trim();
        if rest == "[DONE]" {
            return String::new();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(rest) else {
            return String::new();
        };
        // 事件名优先级：载荷内的 type 字段 > 前一行 event:
        let event_name = chunk
            .get("type")
            .or_else(|| chunk.get("event"))
            .or_else(|| chunk.get("event_type"))
            .or_else(|| chunk.get("eventType"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.pending_event.take());
        self.process(event_name.as_deref().unwrap_or(""), &chunk)
    }

    fn process(&mut self, event_type: &str, chunk: &Value) -> String {
        let normalized = normalize_event_name(event_type);

        match normalized.as_str() {
            "output" | "text" => {
                let text = chunk
                    .get("content")
                    .and_then(|c| c.as_str())
                    .or_else(|| chunk.get("response").and_then(|c| c.as_str()))
                    .unwrap_or("");
                if text.is_empty() {
                    return String::new();
                }
                self.emitted_output = true;
                self.text_chunk(text)
            }
            "reasoning" => {
                let text = chunk
                    .get("reasoning")
                    .and_then(|c| c.as_str())
                    .or_else(|| chunk.get("reasoning_content").and_then(|c| c.as_str()))
                    .or_else(|| chunk.get("thought").and_then(|c| c.as_str()))
                    .unwrap_or("");
                if text.is_empty() {
                    return String::new();
                }
                self.emitted_output = true;
                // 用 reasoning_content 字段透出思维链（OpenAI 兼容客户端多半忽略）
                self.delta_chunk(json!({"reasoning_content": text}))
            }
            "tool_calls" => {
                let Some(calls) = chunk.get("tool_calls").and_then(|c| c.as_array()) else {
                    return String::new();
                };
                let mut out = String::new();
                for (i, tc) in calls.iter().enumerate() {
                    let name = tc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .or_else(|| tc.pointer("/function/name").and_then(|n| n.as_str()))
                        .or_else(|| tc.get("tool_name").and_then(|n| n.as_str()))
                        .unwrap_or("");
                    let args = tc
                        .get("params")
                        .or_else(|| tc.get("arguments"))
                        .or_else(|| tc.get("input"))
                        .or_else(|| tc.pointer("/function/arguments"))
                        .map(stringify_args)
                        .unwrap_or_else(|| "{}".to_string());
                    let id = tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let id = if id.is_empty() {
                        format!("call_{}", crate::rand_hex(8))
                    } else {
                        id
                    };
                    let index = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(i as i64);
                    self.emitted_output = true;
                    out.push_str(&self.delta_chunk(json!({
                        "tool_calls": [{
                            "index": index,
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": args},
                        }]
                    })));
                }
                out
            }
            "token_usage" => {
                let usage = chunk.get("data").cloned().unwrap_or_else(|| chunk.clone());
                self.usage = Some(map_usage(&usage));
                String::new()
            }
            "done" => {
                self.finish_reason = Some(
                    chunk
                        .get("finish_reason")
                        .and_then(|f| f.as_str())
                        .unwrap_or("stop")
                        .to_string(),
                );
                self.final_chunk()
            }
            "error" => {
                let message = chunk
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream error")
                    .to_string();
                let code = chunk
                    .get("code")
                    .map(|c| match c {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "unknown".to_string());
                self.error = Some((message.clone(), code.clone()));
                let status = error_code_to_http(&code);
                format!(
                    "data: {}\n\n",
                    json!({"error": {"message": message, "type": "upstream_error", "code": status}})
                )
            }
            // queue_wait / metadata / timing_cost / progress 等：忽略
            _ => String::new(),
        }
    }

    fn text_chunk(&mut self, text: &str) -> String {
        self.delta_chunk(json!({"content": text}))
    }

    fn delta_chunk(&mut self, delta: Value) -> String {
        self.started = true;
        let mut delta = delta;
        if let Some(obj) = delta.as_object_mut() {
            obj.entry("role".to_string())
                .or_insert_with(|| json!("assistant"));
        }
        format!(
            "data: {}\n\n",
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": null}],
            })
        )
    }

    fn final_chunk(&mut self) -> String {
        let reason = self.finish_reason.clone().unwrap_or_else(|| "stop".to_string());
        let mut out = String::new();
        out.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                "usage": self.usage,
            })
        ));
        out.push_str("data: [DONE]\n\n");
        out
    }

    /// 流意外中断时补一个结束块（幂等）。
    pub fn finish(&mut self) -> String {
        if self.finish_reason.is_some() {
            return String::new();
        }
        self.finish_reason = Some("stop".to_string());
        self.final_chunk()
    }
}

fn normalize_event_name(name: &str) -> String {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "modelconfig" => "model_config".to_string(),
        "planitem" => "plan_item".to_string(),
        "responsedone" | "response_done" | "streamdone" | "stream_done" => "done".to_string(),
        "tokenusage" => "token_usage".to_string(),
        "requestwaitingqueue" => "request_wait_in_queue".to_string(),
        other => other.to_string(),
    }
}

fn stringify_args(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Trae usage → OpenAI usage。
fn map_usage(usage: &Value) -> Value {
    let pick = |keys: &[&str]| -> i64 {
        for k in keys {
            if let Some(n) = usage.get(*k).and_then(|v| v.as_i64()) {
                return n;
            }
        }
        0
    };
    let prompt = pick(&["prompt_tokens", "input_tokens", "input_token", "inputTokens"]);
    let completion = pick(&[
        "completion_tokens",
        "output_tokens",
        "output_token",
        "outputTokens",
    ]);
    let total = pick(&["total_tokens", "total_token", "totalTokens"]);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": if total > 0 { total } else { prompt + completion },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv() -> TraeEventConverter {
        TraeEventConverter::new("glm-5.2")
    }

    fn data_events(text: &str) -> Vec<Value> {
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|d| *d != "[DONE]")
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .collect()
    }

    #[test]
    fn test_text_event_new_format() {
        let mut c = conv();
        let out = c.feed_line(r#"data: {"type":"text","content":"hello"}"#);
        let events = data_events(&out);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "hello");
        assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(c.emitted_output);
    }

    #[test]
    fn test_text_event_old_format() {
        let mut c = conv();
        let out = c.feed_line(r#"data: {"type":"output","response":"legacy"}"#);
        let events = data_events(&out);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "legacy");
    }

    #[test]
    fn test_reasoning_event() {
        let mut c = conv();
        let out = c.feed_line(r#"data: {"type":"reasoning","reasoning":"thinking..."}"#);
        let events = data_events(&out);
        assert_eq!(
            events[0]["choices"][0]["delta"]["reasoning_content"],
            "thinking..."
        );
    }

    #[test]
    fn test_tool_calls_event() {
        let mut c = conv();
        let out = c.feed_line(
            r#"data: {"type":"tool_calls","tool_calls":[{"id":"c1","name":"Bash","params":{"cmd":"ls"}}]}"#,
        );
        let events = data_events(&out);
        let tc = &events[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "c1");
        assert_eq!(tc["function"]["name"], "Bash");
        let args: Value = serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"cmd": "ls"}));
    }

    #[test]
    fn test_token_usage_and_done() {
        let mut c = conv();
        c.feed_line(r#"data: {"type":"token_usage","data":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#);
        let out = c.feed_line(r#"data: {"type":"done","finish_reason":"stop"}"#);
        let events = data_events(&out);
        let last = events.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["prompt_tokens"], 10);
        assert_eq!(last["usage"]["total_tokens"], 15);
        assert!(out.contains("data: [DONE]"));
    }

    #[test]
    fn test_error_event_maps_status() {
        let mut c = conv();
        let out = c.feed_line(r#"data: {"type":"error","code":4008,"message":"quota"}"#);
        let events = data_events(&out);
        assert_eq!(events[0]["error"]["code"], 402);
        assert_eq!(events[0]["error"]["message"], "quota");
        assert!(c.error.is_some());
    }

    #[test]
    fn test_finish_is_idempotent() {
        let mut c = conv();
        c.feed_line(r#"data: {"type":"done"}"#);
        assert_eq!(c.finish(), "");
        let mut c2 = conv();
        assert!(c2.finish().contains("[DONE]"));
    }

    #[test]
    fn test_comment_and_unknown_ignored() {
        let mut c = conv();
        assert_eq!(c.feed_line(": keepalive"), "");
        assert_eq!(c.feed_line(r#"data: {"type":"queue_wait","position":3}"#), "");
    }

    /// 实测格式：事件名在 `event:` 行，data 是纯 JSON（不含 type 字段）。
    #[test]
    fn test_real_wire_format_event_line() {
        let mut c = conv();
        // id: / event: / data: 三行一组
        assert_eq!(c.feed_line("id:1"), "");
        assert_eq!(c.feed_line("event:output"), "");
        let out = c.feed_line(r#"data:{"content":"hello"}"#);
        let events = data_events(&out);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "hello");
    }

    #[test]
    fn test_real_wire_format_error_event() {
        let mut c = conv();
        c.feed_line("id:1");
        c.feed_line("event:error");
        let out = c.feed_line(
            r#"data:{"code":4001,"message":"We're sorry, the param is invalid.","extra":null}"#,
        );
        let events = data_events(&out);
        assert_eq!(events[0]["error"]["code"], 400);
        assert!(c.error.is_some());
    }

    #[test]
    fn test_real_wire_format_done_event() {
        let mut c = conv();
        c.feed_line("id:2");
        c.feed_line("event:done");
        let out = c.feed_line(r#"data:{"finish_reason":"stop"}"#);
        assert!(out.contains("[DONE]"));
        assert_eq!(c.finish(), ""); // done 后再 finish 应幂等
    }

    /// event 名在 `event:` 行、但载荷里也带 type 时，以载荷为准。
    #[test]
    fn test_payload_type_takes_precedence() {
        let mut c = conv();
        c.feed_line("event:unknown_thing");
        let out = c.feed_line(r#"data:{"type":"text","content":"via type"}"#);
        let events = data_events(&out);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "via type");
    }

    #[test]
    fn test_normalize_messages_content_to_array() {
        let msgs = json!([
            {"role": "user", "content": "plain string"},
            {"role": "developer", "content": [{"type": "text", "text": "sys"}]},
        ]);
        let out = normalize_messages(&msgs);
        assert_eq!(out[0]["content"], json!([{"type": "text", "text": "plain string"}]));
        assert_eq!(out[1]["role"], "system");
        assert_eq!(out[1]["content"][0]["text"], "sys");
    }

    #[test]
    fn test_build_request_body_injects_required_fields() {
        let chat = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.5,
        });
        let body = build_request_body(&chat, DEFAULT_FUNCTION);
        assert_eq!(body["function"], "solo_work_lite");
        assert_eq!(body["model"], "DeepSeek-V4-Pro");
        assert_eq!(body["config_name"], "DeepSeek-V4-Pro");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn test_model_alias_passthrough() {
        assert_eq!(resolve_model_name("kimi-k3"), "kimi-k3");
        assert_eq!(resolve_model_name("AUTO"), "glm-5.2");
        assert_eq!(resolve_model_name("gpt-4o"), "DeepSeek-V4-Pro");
    }

    #[test]
    fn test_error_code_mapping() {
        assert_eq!(error_code_to_http("1001"), 401);
        assert_eq!(error_code_to_http("4008"), 402);
        assert_eq!(error_code_to_http("4001"), 400);
        assert_eq!(error_code_to_http("4011"), 429);
        assert_eq!(error_code_to_http("9999"), 502);
    }
}
