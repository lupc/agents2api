//! Trae 私有协议：请求头 / 请求体构造与 SSE 事件 → OpenAI chunk 转换。
//!
//! 主通道是 IDE Agent 单请求端点 `POST {chatHost}/api/agent/v3/llm_utils_chat`。
//!
//! 协议细节参考 `smart-open/TraeWorkAssistant`（Rust 实现，已在真机验证）：
//! - **鉴权头是 `x-ide-token`（裸 JWT）**，不是 `Authorization: Cloud-IDE-JWT`。
//!   带 `Cloud-IDE-JWT ` 前缀会被网关在 token 校验前直接拒绝（401 / code 1001）。
//! - 版本号在 **header**（`x-app-version-code` / `x-ide-version-code`），不在 body。
//! - 请求体有多个必填字段，尤其 `prompt_max_tokens`（固定 168000，
//!   缺失或传 1M 都会返回 4001）。
//! - 模型名需映射为 `config_name`（展示名）+ `model_name`（内部名，形如 `xxx__dev`）。
//! - 响应永远是 SSE；文本增量在 `response` 字段（不是 `content`）；
//!   事件名只从 `event:` 行取（payload 的 `type` 字段不参与判定）。

use serde_json::{json, Map, Value};

use super::auth::TraeAuthInfo;

pub const DEFAULT_CHAT_HOST: &str = "https://trae-api-cn.mchost.guru";
pub const AGENT_PATH: &str = "/api/agent/v3/llm_utils_chat";
pub const DEFAULT_APP_ID: &str = "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8";
/// Trae 客户端版本（`x-ide-version` 与 body 的 `ide_version`）。
pub const DEFAULT_IDE_VERSION: &str = "0.1.50";
/// 版本号代码（`x-app-version-code` / `x-ide-version-code` 与 body 的 `ide_version_code`）。
/// 真机验证值；可用 `TRAE_IDE_VERSION_CODE` 覆盖。
pub const DEFAULT_IDE_VERSION_CODE: &str = "20260811";
/// 上游必填的提示词上限。实测：缺失 → 4001；设为 1M → 4001；168000 → 200。
pub const PROMPT_MAX_TOKENS: i64 = 168_000;
pub const DEFAULT_FUNCTION: &str = "solo_work_lite";
/// 仅在 `solo_agent` 视图下可用的模型。
const SOLO_AGENT_MODELS: &[&str] = &[
    "doubao-seed-code",
    "glm-5.3-flash",
    "qwen3.8-flash",
    "deepseek-v4.1-flash",
    "kimi-k2.8-preview",
];

/// 内置模型兜底列表（无法动态拉取时使用）。
pub const DEFAULT_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-v4.1-flash",
    "glm-5.3",
    "glm-5.2",
    "glm-5-turbo",
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.6",
    "doubao-seed-code",
    "qwen3.8-max",
    "qwen-3.7-plus",
    "minimax-m3",
];

/// 模型显示名 →（canonical `config_name`, 内部 `model_name`）。
/// 大小写不敏感。未收录返回 `None`，调用方按 `model` + `model__dev` 透传。
fn model_config(model: &str) -> Option<(&'static str, String)> {
    let pair: (&'static str, &'static str) = match model.to_lowercase().as_str() {
        "doubao-seed-evolving" => ("Doubao-Seed-Evolving", "Doubao-Seed-Evolving__dev"),
        "doubao-seed-2.1-pro" | "seed-code-pro-0430" => {
            ("Doubao-Seed-2.1-Pro", "Doubao-Seed-2.1-Pro__dev")
        }
        "doubao-seed-2.1-turbo" => ("Doubao-Seed-2.1-Turbo", "Doubao-Seed-2.1-Turbo__dev"),
        "doubao-seed-code" => ("Doubao-Seed-Code", "Doubao-Seed-Code__dev"),
        "glm-5.3-flash" => ("glm-5.3-flash", "glm-5.3-flash__dev"),
        "qwen3.8-flash" => ("qwen3.8-flash", "qwen3.8-flash__dev"),
        "glm-5.2" => ("glm-5.2", "glm-5.2__dev"),
        "glm-5.3" => ("glm-5.3", "glm-5.3__dev"),
        "glm-5" => ("glm-5", "glm-5__dev"),
        "glm-5-turbo" => ("glm-5-turbo", "glm-5-turbo__dev"),
        "deepseek-v4.1-flash" => ("deepseek-v4.1-flash", "deepseek-v4.1-flash__dev"),
        "deepseek-v4-flash" => ("DeepSeek-V4-Flash", "deepseek_v4_flash__dev"),
        "deepseek-v4-flash-official" => {
            ("DeepSeek-V4-Flash-Official", "DeepSeek-V4-Flash-Official__dev")
        }
        "deepseek-v4-pro" => ("DeepSeek-V4-Pro", "deepseek_v4_pro__dev"),
        "deepseek-v4-pro-official" => {
            ("DeepSeek-V4-Pro-Official", "DeepSeek-V4-Pro-Official__dev")
        }
        "kimi-k2.6" => ("kimi-k2.6", "kimi-k2.6__dev"),
        "kimi-k2.7-code" => ("kimi-k2.7-code", "kimi-k2.7-code__dev"),
        "kimi-k2.8-preview" => ("kimi-k2.8-preview", "kimi-k2.8-preview__dev"),
        "kimi-k3" => ("kimi-k3", "kimi-k3__dev"),
        "minimax-m3" => ("minimax-m3", "minimax-m3__dev"),
        "qwen3.8-max" => ("qwen3.8-max", "qwen3.8-max__dev"),
        "qwen-3.7-plus" => ("qwen-3.7-plus", "qwen-3.7-plus__dev"),
        _ => return None,
    };
    Some((pair.0, pair.1.to_string()))
}

/// 解析模型名 → (`config_name`, `model_name`)。
pub fn resolve_model(model: &str) -> (String, String) {
    let model = model.trim();
    match model_config(model) {
        Some((c, m)) => (c.to_string(), m),
        // 未收录模型按实证规律原样透传：model_name = config_name__dev
        None => (model.to_string(), format!("{model}__dev")),
    }
}

/// 该模型应该用哪个 `function` 视图。部分模型只在 `solo_agent` 下可用。
pub fn function_for_model(model: &str) -> &'static str {
    let lower = model.to_lowercase();
    if SOLO_AGENT_MODELS.contains(&lower.as_str()) {
        "solo_agent"
    } else {
        DEFAULT_FUNCTION
    }
}

/// 把 OpenAI messages 转成 Trae 需要的形式：
/// - `content` 一律转成 `[{"type":"text","text":...}]`（Trae 硬性要求数组）
/// - `developer` → `system`
/// - assistant 的 `tool_calls[].function` → `function_call`，无名调用丢弃
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
        let mut msg = Map::new();
        msg.insert("role".to_string(), json!(role));
        msg.insert(
            "content".to_string(),
            normalize_content(m.get("content").cloned().unwrap_or(Value::Null)),
        );

        // assistant 的 tool_calls：function → function_call，去除空名
        if role == "assistant" {
            if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                let kept: Vec<Value> = tcs
                    .iter()
                    .filter_map(|tc| {
                        let mut tcm = tc.as_object()?.clone();
                        if let Some(f) = tcm.remove("function") {
                            tcm.insert("function_call".to_string(), f);
                        }
                        let has_name = tcm
                            .get("function_call")
                            .and_then(|fc| fc.get("name"))
                            .and_then(|n| n.as_str())
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false);
                        if has_name {
                            Some(Value::Object(tcm))
                        } else {
                            None
                        }
                    })
                    .collect();
                if !kept.is_empty() {
                    msg.insert("tool_calls".to_string(), Value::Array(kept));
                }
            }
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

/// 规范化 `tool_choice`；`none` 时移除 tools。
fn normalize_tool_choice(obj: &mut Map<String, Value>) {
    let Some(choice) = obj.get("tool_choice").cloned() else {
        return;
    };
    match &choice {
        Value::String(s) if s == "none" => {
            obj.remove("tools");
            obj.insert("tool_choice".to_string(), json!("none"));
        }
        Value::String(s) => {
            obj.insert("tool_choice".to_string(), json!(s));
        }
        Value::Object(_) => {
            let name = choice
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .or_else(|| choice.get("name").and_then(|n| n.as_str()))
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                obj.insert("tool_choice".to_string(), json!("auto"));
            } else {
                obj.insert("tool_choice".to_string(), json!(name));
            }
        }
        _ => {}
    }
}

/// tool schema 的 `parameters` 必须是 **JSON 字符串**（上游要求）。
fn normalize_tools(obj: &mut Map<String, Value>) {
    let Some(tools) = obj.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return;
    };
    for tool in tools.iter_mut() {
        let Some(tm) = tool.as_object_mut() else { continue };
        // 顶层若是 Responses 扁平格式，转成嵌套 function
        if tm.get("function").is_none() && tm.get("type").and_then(|t| t.as_str()) == Some("function")
        {
            let mut f = Map::new();
            for k in ["name", "description", "parameters", "strict"] {
                if let Some(v) = tm.get(k) {
                    f.insert(k.to_string(), v.clone());
                }
            }
            let mut nt = Map::new();
            nt.insert("type".to_string(), json!("function"));
            nt.insert("function".to_string(), Value::Object(f));
            *tm = nt;
        }
        if let Some(params) = tm
            .get_mut("function")
            .and_then(|f| f.as_object_mut())
            .and_then(|f| f.get_mut("parameters"))
        {
            if params.is_object() {
                let s = params.to_string();
                *params = json!(s);
            }
        }
    }
}

/// 构造发往 Trae 的请求体（在标准 OpenAI Chat body 基础上包装）。
pub fn build_request_body(chat: &Value, info: &TraeAuthInfo) -> Value {
    let mut obj: Map<String, Value> = chat.as_object().cloned().unwrap_or_default();

    obj.insert(
        "messages".to_string(),
        normalize_messages(chat.get("messages").unwrap_or(&json!([]))),
    );

    let raw_model = chat
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());
    let (config_name, model_name) = resolve_model(&raw_model);

    normalize_tool_choice(&mut obj);
    normalize_tools(&mut obj);

    // llm_utils_chat 必填字段
    obj.insert("config_name".to_string(), json!(config_name));
    obj.insert("model_name".to_string(), json!(model_name));
    obj.insert("model".to_string(), json!(config_name));
    obj.insert("stream".to_string(), json!(true));
    obj.insert(
        "function".to_string(),
        json!(function_for_model(&raw_model)),
    );
    // max_tokens：尊重客户端值，缺省 4096（上游必需）
    let max_tokens = obj
        .get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .map(|n| json!(n))
        .unwrap_or_else(|| json!(4096));
    obj.insert("max_tokens".to_string(), max_tokens);

    obj.insert("conversation_id".to_string(), json!(gen_uuid_like()));
    obj.insert("user_id".to_string(), json!(info.user_id));
    obj.insert("session_id".to_string(), json!(gen_uuid_like()));
    obj.insert("device_id".to_string(), json!(info.device_id));
    obj.insert("machine_id".to_string(), json!(info.machine_id));
    obj.insert("project_id".to_string(), json!(gen_uuid_like()));
    obj.insert("workspace_id".to_string(), json!("e04cdd"));
    // 上游必填；168000 是实测可用值（缺失或 1M 均 4001）
    obj.insert("prompt_max_tokens".to_string(), json!(PROMPT_MAX_TOKENS));
    obj.insert("mode".to_string(), json!("FunctionCall"));
    obj.insert("ide_version".to_string(), json!(ide_version()));
    obj.insert("ide_version_code".to_string(), json!(ide_version_code()));
    obj.insert("app_id".to_string(), json!(app_id()));
    obj.insert("package_type".to_string(), json!("stable_cn"));

    Value::Object(obj)
}

fn app_id() -> String {
    std::env::var("TRAE_APP_ID").unwrap_or_else(|_| DEFAULT_APP_ID.to_string())
}

fn ide_version() -> String {
    std::env::var("TRAE_IDE_VERSION").unwrap_or_else(|_| DEFAULT_IDE_VERSION.to_string())
}

fn ide_version_code() -> String {
    std::env::var("TRAE_IDE_VERSION_CODE").unwrap_or_else(|_| DEFAULT_IDE_VERSION_CODE.to_string())
}

fn gen_uuid_like() -> String {
    format!(
        "{}-{}-{}-{}-{}",
        crate::rand_hex(4),
        crate::rand_hex(2),
        crate::rand_hex(2),
        crate::rand_hex(2),
        crate::rand_hex(6)
    )
}

/// 构造发往 Trae 的请求头（照搬真机验证的头集合）。
///
/// 关键：鉴权用 **`x-ide-token` 裸 JWT**，不发 `Authorization`。
pub fn build_headers(info: &TraeAuthInfo, _stream: bool) -> Vec<(String, String)> {
    let token = &info.token;
    let trace_id = format!("00-{}-{}-01", gen_uuid_like(), gen_uuid_like());
    let flow_traceparent = format!(
        "04-{}-{}-01",
        &trace_id[3..trace_id.len().min(35)],
        gen_uuid_like()
    );
    let request_id = format!("req_{}", gen_uuid_like());

    vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "*/*".to_string()),
        (
            "accept-encoding".to_string(),
            "gzip, deflate, br, zstd".to_string(),
        ),
        ("user-agent".to_string(), "TraeClient/TTNet".to_string()),
        // 鉴权：裸 JWT，不能带 "Cloud-IDE-JWT " 前缀
        ("x-ide-token".to_string(), token.clone()),
        ("x-app-id".to_string(), app_id()),
        ("x-app-version".to_string(), "default".to_string()),
        ("x-app-version-code".to_string(), ide_version_code()),
        ("x-ide-version".to_string(), ide_version()),
        ("x-ide-version-code".to_string(), ide_version_code()),
        ("x-ide-version-type".to_string(), "stable".to_string()),
        ("x-device-type".to_string(), device_type().to_string()),
        ("x-device-brand".to_string(), "CREFG-XX".to_string()),
        ("x-device-cpu".to_string(), "Intel".to_string()),
        ("x-device-id".to_string(), info.device_id.clone()),
        ("x-machine-id".to_string(), info.machine_id.clone()),
        (
            "x-os-version".to_string(),
            "Windows 11 Home China".to_string(),
        ),
        ("request-traffic-type".to_string(), "prod".to_string()),
        ("package-type".to_string(), "stable_cn".to_string()),
        ("x-lgw-req-sdk-type".to_string(), "3".to_string()),
        ("x-lscbd-aid".to_string(), "787976".to_string()),
        ("x-lscbd-platform".to_string(), "windows".to_string()),
        ("x-ss-dp".to_string(), "787976".to_string()),
        ("app-version".to_string(), ide_version()),
        (
            "x-custom-trace-id".to_string(),
            trace_id[..trace_id.len().min(16)].to_string(),
        ),
        ("x-flow-traceparent".to_string(), flow_traceparent),
        ("x-tt-trace-id".to_string(), trace_id),
        ("x-request-id".to_string(), request_id),
        (
            "referer".to_string(),
            format!("{}{}", DEFAULT_CHAT_HOST, AGENT_PATH),
        ),
    ]
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
///
/// `4001`（param is invalid / model config mismatch）是**请求级**错误，
/// 与账号无关：换账号只会复现同样错误并耗尽账号池。这里统一映射为 400。
pub fn error_code_to_http(code: &str) -> u16 {
    match code {
        "1001" => 401,
        "1005" => 402,
        "4008" => 429,
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
/// 实测线格式（Trae CN）：
/// ```text
/// id:1
/// event:output
/// data:{"response":"hello","reasoning_content":"...","tool_calls":[...]}
///
/// ```
/// 事件名**只从 `event:` 行取**；文本增量在 `response` 字段。
pub struct TraeEventConverter {
    model: String,
    id: String,
    created: i64,
    started: bool,
    finish_reason: Option<String>,
    usage: Option<Value>,
    pending_event: Option<String>,
    /// 已见过的 tool_call 下标（Trae 把 id/name 与 arguments 拆成多个增量块，
    /// 首个块给出 id/name，后续块只带 arguments）
    seen_tool_calls: std::collections::BTreeSet<i64>,
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
            seen_tool_calls: std::collections::BTreeSet::new(),
            emitted_output: false,
            error: None,
        }
    }

    /// 处理一行 Trae SSE，返回要发给客户端的 OpenAI SSE 文本。
    pub fn feed_line(&mut self, line: &str) -> String {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') {
            return String::new();
        }
        if let Some(rest) = line.strip_prefix("id:") {
            let _ = rest;
            return String::new();
        }
        // 事件名只从 event: 行取
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
        let chunk: Value = if rest.is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(rest) {
                Ok(v) => v,
                Err(_) => return String::new(),
            }
        };
        let event = self.pending_event.take().unwrap_or_default();
        self.process(&event, &chunk)
    }

    fn process(&mut self, event_type: &str, chunk: &Value) -> String {
        match normalize_event_name(event_type).as_str() {
            // 文本增量在 response 字段；reasoning_content 为思维链
            "output" => {
                let text = chunk
                    .get("response")
                    .and_then(|c| c.as_str())
                    .or_else(|| chunk.get("content").and_then(|c| c.as_str()))
                    .unwrap_or("");
                let reasoning = chunk
                    .get("reasoning_content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let mut out = String::new();
                if !text.is_empty() {
                    self.emitted_output = true;
                    out.push_str(&self.delta_chunk(json!({"content": text})));
                }
                if !reasoning.is_empty() {
                    self.emitted_output = true;
                    out.push_str(&self.delta_chunk(json!({"reasoning_content": reasoning})));
                }
                if let Some(tcs) = chunk.get("tool_calls").and_then(|t| t.as_array()) {
                    out.push_str(&self.tool_calls_chunk(tcs));
                }
                out
            }
            // create_agent_task 旧格式，保留兼容
            "thought" => {
                let text = chunk
                    .get("thought")
                    .and_then(|c| c.as_str())
                    .or_else(|| chunk.get("reasoning_content").and_then(|c| c.as_str()))
                    .unwrap_or("");
                if text.is_empty() {
                    return String::new();
                }
                self.emitted_output = true;
                self.delta_chunk(json!({"reasoning_content": text}))
            }
            "token_usage" => {
                self.usage = Some(map_usage(chunk));
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
            "turn_completion" => {
                self.finish_reason = Some("stop".to_string());
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
                    json!({"error": {"message": message, "type": "api_error", "code": status}})
                )
            }
            _ => String::new(),
        }
    }

    fn tool_calls_chunk(&mut self, calls: &[Value]) -> String {
        let mut out = String::new();
        for (i, tc) in calls.iter().enumerate() {
            // Trae 用 function_call 子对象，子字段 namespace/partial_arguments 需剥离
            let fc = tc.get("function_call").or_else(|| tc.get("function"));
            let name = fc
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| tc.get("name").and_then(|n| n.as_str()))
                .unwrap_or("");
            let args = fc
                .and_then(|f| f.get("arguments"))
                .map(|a| match a {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .or_else(|| {
                    tc.get("params").map(|p| match p {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                })
                .unwrap_or_default();
            let index = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(i as i64);
            let first_seen = self.seen_tool_calls.insert(index);
            // id 只在首个块给出；后续块为空
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let id = match (id, first_seen) {
                (Some(id), _) if !id.is_empty() => id,
                (_, true) => format!("call_{}", crate::rand_hex(8)),
                (_, false) => String::new(),
            };

            // 首个块即使 name/args 为空也要发（建立 tool_call 槽位）；后续块只在有内容时发
            if !first_seen && name.is_empty() && args.is_empty() && id.is_empty() {
                continue;
            }
            self.emitted_output = true;
            let mut function = serde_json::Map::new();
            if !name.is_empty() {
                function.insert("name".to_string(), json!(name));
            }
            function.insert("arguments".to_string(), json!(args));
            out.push_str(&self.delta_chunk(json!({
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": function,
                }]
            })));
        }
        out
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
        let reason = self
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string());
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
    let lower = name.trim().to_lowercase();
    match lower.as_str() {
        "modelconfig" => "model_config".to_string(),
        "planitem" => "plan_item".to_string(),
        "responsedone" | "response_done" | "streamdone" | "stream_done" => "done".to_string(),
        "tokenusage" => "token_usage".to_string(),
        "requestwaitingqueue" => "request_wait_in_queue".to_string(),
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
    use std::path::PathBuf;

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

    fn fake_info() -> TraeAuthInfo {
        TraeAuthInfo {
            token: "jwt-abc".to_string(),
            user_id: "89529830872176".to_string(),
            device_id: "12345".to_string(),
            machine_id: "deadbeef".to_string(),
            expires_at_ms: 0,
            host_variant: "Trae CN".to_string(),
            storage_path: PathBuf::from("/fake/storage.json"),
        }
    }

    /// 实测线格式：文本增量在 response 字段，事件名只从 event: 行取。
    #[test]
    fn test_real_wire_output_event() {
        let mut c = conv();
        c.feed_line("id:1");
        c.feed_line("event:output");
        let out = c.feed_line(r#"data:{"response":"hello"}"#);
        let events = data_events(&out);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "hello");
        assert_eq!(events[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(c.emitted_output);
    }

    /// 空 JSON 的 output 块是真实世界常态，不应产生任何输出。
    #[test]
    fn test_empty_output_chunk_ignored() {
        let mut c = conv();
        c.feed_line("event:output");
        assert_eq!(c.feed_line("data:{}"), "");
    }

    #[test]
    fn test_output_with_reasoning_and_tools() {
        let mut c = conv();
        c.feed_line("event:output");
        let out = c.feed_line(
            r#"data:{"response":"txt","reasoning_content":"think","tool_calls":[{"index":0,"id":"c1","function_call":{"name":"Bash","arguments":"{\"cmd\":\"ls\"}"}}]}"#,
        );
        let events = data_events(&out);
        assert_eq!(events[0]["choices"][0]["delta"]["content"], "txt");
        assert_eq!(events[1]["choices"][0]["delta"]["reasoning_content"], "think");
        let tc = &events[2]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["id"], "c1");
        assert_eq!(tc["function"]["name"], "Bash");
    }

    /// 实测：Trae 把 tool_call 拆成多个增量块——首块给 id+name，次块只给 arguments。
    /// 后续块不能覆盖首块的 name（回归：曾导致 name 变空）。
    #[test]
    fn test_tool_calls_split_across_chunks() {
        let mut c = conv();
        c.feed_line("event:output");
        let out1 = c.feed_line(
            r#"data:{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function_call":{"name":"Bash","arguments":"","partial_arguments":null,"namespace":null}}]}"#,
        );
        c.feed_line("event:output");
        let out2 = c.feed_line(
            r#"data:{"tool_calls":[{"index":0,"id":"","type":"function","function_call":{"name":"","arguments":"{\"cmd\": \"ls -la\"}","partial_arguments":null,"namespace":null}}]}"#,
        );

        let e1 = data_events(&out1);
        let tc1 = &e1[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc1["id"], "call_abc");
        assert_eq!(tc1["function"]["name"], "Bash");

        let e2 = data_events(&out2);
        let tc2 = &e2[0]["choices"][0]["delta"]["tool_calls"][0];
        // 第二块 name 为空，不应覆盖首块已建立的名字
        assert!(tc2["function"].get("name").is_none());
        assert_eq!(tc2["function"]["arguments"], "{\"cmd\": \"ls -la\"}");
    }

    /// payload 里的 type 字段不参与事件判定（与参考实现一致）。
    #[test]
    fn test_payload_type_field_ignored() {
        let mut c = conv();
        c.feed_line("event:output");
        let out = c.feed_line(r#"data:{"type":"text","response":"via response"}"#);
        let events = data_events(&out);
        assert_eq!(
            events[0]["choices"][0]["delta"]["content"],
            "via response"
        );
    }

    #[test]
    fn test_thought_legacy_event() {
        let mut c = conv();
        c.feed_line("event:thought");
        let out = c.feed_line(r#"data:{"thought":"legacy thinking"}"#);
        let events = data_events(&out);
        assert_eq!(
            events[0]["choices"][0]["delta"]["reasoning_content"],
            "legacy thinking"
        );
    }

    #[test]
    fn test_token_usage_and_done() {
        let mut c = conv();
        c.feed_line("event:token_usage");
        c.feed_line(r#"data:{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#);
        c.feed_line("event:done");
        let out = c.feed_line(r#"data:{"finish_reason":"stop"}"#);
        let events = data_events(&out);
        let last = events.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["prompt_tokens"], 10);
        assert_eq!(last["usage"]["total_tokens"], 15);
        assert!(out.contains("data: [DONE]"));
    }

    #[test]
    fn test_turn_completion_legacy() {
        let mut c = conv();
        c.feed_line("event:turn_completion");
        let out = c.feed_line("data:{}");
        assert!(out.contains("\"finish_reason\":\"stop\""));
        assert!(out.contains("data: [DONE]"));
    }

    #[test]
    fn test_error_event_maps_status() {
        let mut c = conv();
        c.feed_line("event:error");
        let out = c.feed_line(
            r#"data:{"code":4001,"message":"model config is empty"}"#,
        );
        let events = data_events(&out);
        assert_eq!(events[0]["error"]["code"], 400);
        assert_eq!(events[0]["error"]["message"], "model config is empty");
        assert!(c.error.is_some());
    }

    #[test]
    fn test_finish_is_idempotent() {
        let mut c = conv();
        c.feed_line("event:done");
        c.feed_line("data:{}");
        assert_eq!(c.finish(), "");
        let mut c2 = conv();
        assert!(c2.finish().contains("[DONE]"));
    }

    #[test]
    fn test_comment_and_unknown_ignored() {
        let mut c = conv();
        assert_eq!(c.feed_line(": keepalive"), "");
        c.feed_line("event:metadata");
        assert_eq!(c.feed_line(r#"data:{"session_id":"x"}"#), "");
        c.feed_line("event:timing_cost");
        assert_eq!(c.feed_line(r#"data:{"cost":1}"#), "");
    }

    #[test]
    fn test_normalize_messages_content_to_array() {
        let msgs = json!([
            {"role": "user", "content": "plain string"},
            {"role": "developer", "content": [{"type": "text", "text": "sys"}]},
        ]);
        let out = normalize_messages(&msgs);
        assert_eq!(
            out[0]["content"],
            json!([{"type": "text", "text": "plain string"}])
        );
        assert_eq!(out[1]["role"], "system");
        assert_eq!(out[1]["content"][0]["text"], "sys");
    }

    #[test]
    fn test_normalize_messages_tool_calls_renamed() {
        let msgs = json!([
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "Bash", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "", "arguments": "{}"}},
            ]},
        ]);
        let out = normalize_messages(&msgs);
        let tcs = out[0]["tool_calls"].as_array().unwrap();
        // 空名的被丢弃，function → function_call
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["function_call"]["name"], "Bash");
        assert!(tcs[0].get("function").is_none());
    }

    /// 关键回归：必填字段齐全，尤其是 prompt_max_tokens。
    #[test]
    fn test_build_request_body_required_fields() {
        let chat = json!({
            "model": "deepseek-v4-flash",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let body = build_request_body(&chat, &fake_info());
        assert_eq!(body["config_name"], "DeepSeek-V4-Flash");
        assert_eq!(body["model_name"], "deepseek_v4_flash__dev");
        assert_eq!(body["prompt_max_tokens"], 168000);
        assert_eq!(body["stream"], true);
        assert_eq!(body["function"], "solo_work_lite");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["mode"], "FunctionCall");
        assert_eq!(body["workspace_id"], "e04cdd");
        assert_eq!(body["package_type"], "stable_cn");
        assert_eq!(body["user_id"], "89529830872176");
        assert_eq!(body["device_id"], "12345");
        assert_eq!(body["machine_id"], "deadbeef");
        assert_eq!(body["ide_version"], "0.1.50");
        assert_eq!(body["ide_version_code"], "20260811");
        // 必填的 id 字段都存在且非空
        for k in ["conversation_id", "session_id", "project_id"] {
            assert!(
                body[k].as_str().map(|s| !s.is_empty()).unwrap_or(false),
                "{k} 应非空"
            );
        }
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn test_model_view_function_selection() {
        // deepseek-v4.1-flash 只在 solo_agent 下可用
        assert_eq!(function_for_model("deepseek-v4.1-flash"), "solo_agent");
        assert_eq!(function_for_model("doubao-seed-code"), "solo_agent");
        assert_eq!(function_for_model("glm-5.2"), "solo_work_lite");
        assert_eq!(function_for_model("deepseek-v4-flash"), "solo_work_lite");
    }

    #[test]
    fn test_model_resolution() {
        assert_eq!(
            resolve_model("deepseek-v4-flash"),
            ("DeepSeek-V4-Flash".to_string(), "deepseek_v4_flash__dev".to_string())
        );
        assert_eq!(
            resolve_model("kimi-k3"),
            ("kimi-k3".to_string(), "kimi-k3__dev".to_string())
        );
        // 未收录模型：原样透传 + __dev 后缀
        assert_eq!(
            resolve_model("brand-new-model"),
            ("brand-new-model".to_string(), "brand-new-model__dev".to_string())
        );
        // 大小写不敏感
        assert_eq!(resolve_model("DeepSeek-V4-Flash").0, "DeepSeek-V4-Flash");
    }

    /// 关键回归：鉴权头是裸 JWT 的 x-ide-token，不出现 Authorization。
    #[test]
    fn test_headers_use_bare_jwt_not_authorization() {
        let headers = build_headers(&fake_info(), true);
        let map: std::collections::HashMap<_, _> = headers.iter().cloned().collect();
        assert_eq!(map.get("x-ide-token").unwrap(), "jwt-abc");
        assert!(!headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")));
        assert!(!map.get("x-ide-token").unwrap().starts_with("Cloud-IDE-JWT"));
        // 版本号在 header
        assert_eq!(map.get("x-app-version-code").unwrap(), "20260811");
        assert_eq!(map.get("x-ide-version-code").unwrap(), "20260811");
        assert_eq!(map.get("x-ide-version").unwrap(), "0.1.50");
        // 关键头存在
        for k in [
            "x-app-id",
            "x-device-id",
            "x-machine-id",
            "referer",
            "x-flow-traceparent",
            "user-agent",
        ] {
            assert!(map.contains_key(k), "缺少头 {k}");
        }
        assert_eq!(map.get("user-agent").unwrap(), "TraeClient/TTNet");
    }

    #[test]
    fn test_tool_parameters_serialized_to_string() {
        let chat = json!({
            "model": "glm-5.2",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {
                "name": "Bash",
                "description": "run",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }}],
        });
        let body = build_request_body(&chat, &fake_info());
        // parameters 必须是 JSON 字符串
        assert!(body["tools"][0]["function"]["parameters"].is_string());
    }

    #[test]
    fn test_error_code_mapping() {
        assert_eq!(error_code_to_http("1001"), 401);
        assert_eq!(error_code_to_http("4001"), 400);
        assert_eq!(error_code_to_http("4023"), 400);
        assert_eq!(error_code_to_http("4008"), 429);
        assert_eq!(error_code_to_http("9999"), 502);
    }
}

#[cfg(test)]
mod agg_tests {
    use super::*;

    /// 复现完整链路：分片 tool_calls 私有 SSE → 转换 → 标准聚合。
    #[test]
    fn test_split_tool_calls_through_aggregation() {
        let mut conv = TraeEventConverter::new("trae");
        let mut out = String::new();
        for line in [
            "id:1", "event:output",
            "data:{}",
            "id:2", "event:output",
            r#"data:{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function_call":{"name":"Bash","arguments":"","partial_arguments":null,"namespace":null}}]}"#,
            "id:3", "event:output",
            r#"data:{"tool_calls":[{"index":0,"id":"","type":"function","function_call":{"name":"","arguments":"{\"cmd\": \"ls -la\"}","partial_arguments":null,"namespace":null}}]}"#,
            "id:4", "event:done", r#"data:{"finish_reason":"stop"}"#,
        ] {
            out.push_str(&conv.feed_line(line));
        }
        out.push_str(&conv.finish());

        let agg = crate::upstream::aggregate_openai_sse(out.as_bytes());
        let tc = &agg["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "Bash", "聚合后 name 应保留；实际: {agg}");
        assert_eq!(tc["id"], "call_abc");
        assert_eq!(tc["function"]["arguments"], "{\"cmd\": \"ls -la\"}");
        assert_eq!(agg["choices"][0]["finish_reason"], "tool_calls");
    }
}
