//! Anthropic Messages API ↔ OpenAI Chat Completions API 适配层。
//!
//! Claude Code / CC Switch 使用 Anthropic Messages API（POST /v1/messages），
//! 而 CodeBuddy 后端只支持 OpenAI Chat Completions 协议。本模块做双向转换：
//!   请求：Anthropic Messages 格式 → OpenAI Chat 格式
//!   响应：OpenAI Chat SSE → Anthropic Messages SSE 事件流

use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::rand_hex;

fn rand_id(prefix: &str) -> String {
    format!("{prefix}{}", rand_hex(12))
}

// ---------------------------------------------------------------------------
// 请求转换：Anthropic → Chat
// ---------------------------------------------------------------------------

/// 提取 system 字段为纯文本字符串。支持 string 和 [{type:text, text:...}] 数组。
fn extract_system_text(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// 从 content blocks 中提取所有 text 块合并为字符串。
fn extract_blocks_text(blocks: &Value) -> String {
    if let Value::Array(items) = blocks {
        let parts: Vec<&str> = items
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect();
        return parts.concat();
    }
    String::new()
}

/// Map Anthropic text/image blocks to Chat, preserving image bytes and order.
fn convert_image_content(content: &Value) -> Result<Value, String> {
    match content {
        Value::String(s) => Ok(json!(s)),
        Value::Null => Ok(json!("")),
        Value::Array(blocks) => {
            let mut parts: Vec<Value> = Vec::new();
            let mut has_image = false;
            for block in blocks {
                if !block.is_object() {
                    return Err("Anthropic content blocks must be objects".to_string());
                }
                let kind = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "text" => {
                        parts.push(json!({
                            "type": "text",
                            "text": block.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                        }));
                    }
                    "image" => {
                        let source = block.get("source").cloned().unwrap_or(Value::Null);
                        if !source.is_object() {
                            return Err("Image source must be an object".to_string());
                        }
                        let url = match source.get("type").and_then(|t| t.as_str()) {
                            Some("url") => {
                                let u = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
                                if u.trim().is_empty() {
                                    return Err("Image URL must be non-empty".to_string());
                                }
                                u.to_string()
                            }
                            Some("base64") => {
                                let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                                let media_type =
                                    source.get("media_type").and_then(|m| m.as_str()).unwrap_or("");
                                if data.is_empty()
                                    || !media_type.starts_with("image/")
                                {
                                    return Err(
                                        "Base64 images require data and an image media_type"
                                            .to_string(),
                                    );
                                }
                                format!("data:{media_type};base64,{data}")
                            }
                            _ => {
                                return Err(
                                    "Image source must use url or base64; file references are not supported"
                                        .to_string(),
                                )
                            }
                        };
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        has_image = true;
                    }
                    other => {
                        return Err(format!("Unsupported Anthropic content block: {other:?}"));
                    }
                }
            }
            if has_image {
                Ok(Value::Array(parts))
            } else {
                let text: String = parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect();
                Ok(json!(text))
            }
        }
        _ => Err("Anthropic content must be a string or a list of blocks".to_string()),
    }
}

/// 将单个 Anthropic 消息转换为 OpenAI 格式的消息（可能为多条）。
fn convert_anthropic_message(msg: &Value) -> Result<Vec<Value>, String> {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let content = msg.get("content").cloned().unwrap_or(Value::Null);

    // 简单字符串 content
    if let Value::String(s) = &content {
        return Ok(vec![json!({"role": role, "content": s})]);
    }

    // 空 content
    let Value::Array(blocks) = &content else {
        return Ok(vec![]);
    };
    if blocks.is_empty() {
        return Ok(vec![]);
    }

    if role == "user" {
        let mut result: Vec<Value> = Vec::new();
        let mut user_blocks: Vec<Value> = Vec::new();
        for block in blocks {
            if !block.is_object() {
                continue;
            }
            let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if bt == "tool_result" {
                // tool_result → 独立的 tool 消息
                let tc_id = block.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                let output = convert_image_content(block.get("content").unwrap_or(&Value::Null))?;
                result.push(json!({"role": "tool", "tool_call_id": tc_id, "content": output}));
            } else {
                user_blocks.push(block.clone());
            }
        }
        if !user_blocks.is_empty() {
            // Chat requires tool results immediately after the assistant tool calls.
            let converted = convert_image_content(&Value::Array(user_blocks))?;
            result.push(json!({"role": "user", "content": converted}));
        }
        return Ok(result);
    }

    if role == "assistant" {
        let mut text_parts: Vec<&str> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        for block in blocks {
            if !block.is_object() {
                continue;
            }
            let bt = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match bt {
                "text" => {
                    text_parts.push(block.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                }
                "tool_use" => {
                    let input_val = block.get("input").cloned().unwrap_or(json!({}));
                    let tc = json!({
                        "id": block.get("id").cloned().unwrap_or_else(|| json!(rand_id("call_"))),
                        "type": "function",
                        "function": {
                            "name": block.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                            "arguments": serde_json::to_string(&input_val)
                                .unwrap_or_else(|_| "{}".to_string()),
                        },
                    });
                    tool_calls.push(tc);
                }
                _ => {}
            }
        }
        let mut msg_out = json!({"role": "assistant"});
        if text_parts.is_empty() {
            msg_out["content"] = Value::Null;
        } else {
            msg_out["content"] = json!(text_parts.concat());
        }
        if !tool_calls.is_empty() {
            msg_out["tool_calls"] = Value::Array(tool_calls);
        }
        return Ok(vec![msg_out]);
    }

    // 其他角色：尝试提取文本
    let text = extract_blocks_text(&content);
    if !text.is_empty() {
        return Ok(vec![json!({"role": role, "content": text})]);
    }
    Ok(vec![])
}

/// 将 Anthropic 格式的 tools 转为 OpenAI Chat 格式。
fn convert_anthropic_tools(tools: &Value) -> Value {
    let mut result: Vec<Value> = Vec::new();
    if let Value::Array(items) = tools {
        for t in items {
            if !t.is_object() {
                continue;
            }
            // 已经是 Chat 格式
            if t.get("function").is_some() {
                result.push(t.clone());
                continue;
            }
            let mut fn_obj = serde_json::Map::new();
            fn_obj.insert(
                "name".to_string(),
                json!(t.get("name").and_then(|n| n.as_str()).unwrap_or("")),
            );
            if let Some(d) = t.get("description") {
                fn_obj.insert("description".to_string(), d.clone());
            }
            if let Some(p) = t.get("input_schema") {
                fn_obj.insert("parameters".to_string(), p.clone());
            }
            result.push(json!({"type": "function", "function": fn_obj}));
        }
    }
    Value::Array(result)
}

/// 将 Anthropic Messages API 请求体转换为 OpenAI Chat Completions 请求体。
pub fn anthropic_request_to_chat(body: &Value) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();

    // system → 首条 system 消息
    if let Some(system) = body.get("system") {
        let sys_content = extract_system_text(system);
        if !sys_content.is_empty() {
            messages.push(json!({"role": "system", "content": sys_content}));
        }
    }

    // messages → 消息转换
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if !m.is_object() {
                continue;
            }
            messages.extend(convert_anthropic_message(m)?);
        }
    }

    let mut chat = json!({"messages": messages, "stream": true});

    if let Some(model) = body.get("model") {
        chat["model"] = model.clone();
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        chat["max_tokens"] = max_tokens.clone();
    }
    if let Some(tools) = body.get("tools") {
        if !tools.is_null() {
            let converted = convert_anthropic_tools(tools);
            if !converted.as_array().unwrap().is_empty() {
                chat["tools"] = converted;
            }
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        match tc {
            Value::Object(_) => {
                chat["tool_choice"] = json!({
                    "type": tc.get("type").cloned().unwrap_or(json!("any")),
                    "function": {"name": tc.get("name").and_then(|n| n.as_str()).unwrap_or("")},
                });
            }
            Value::String(s) => {
                chat["tool_choice"] = if ["none", "auto", "required"].contains(&s.as_str()) {
                    json!(s.clone())
                } else {
                    json!({"type": "function", "function": {"name": s}})
                };
            }
            _ => {}
        }
    }

    // 透传常见参数
    for key in ["temperature", "top_p", "stop", "top_k"] {
        if let Some(v) = body.get(key) {
            chat[key] = v.clone();
        }
    }

    // Anthropic 的 thinking / effort → 上游的 reasoning_effort。
    // Anthropic 客户端通过 `thinking` 对象（extended/adaptive）或 `effort` 表达“要思考”，
    // 而上游（CodeBuddy 等）认的是 OpenAI 的 `reasoning_effort`，这里做映射。
    // 注意：部分模型（如 deepseek-v4-flash）只有带上该字段才会输出 reasoning_content。
    map_thinking_to_reasoning_effort(body, &mut chat);

    Ok(chat)
}

/// 把 Anthropic 请求里的思考意图映射为上游的 `reasoning_effort`。
///
/// - 显式 `effort`（low/medium/high/max）优先直接透传（max → high）。
/// - `thinking.type == "enabled"`：按 `budget_tokens` 粗略映射（>=6000 high，否则 medium）。
/// - `thinking.type == "adaptive"`：默认 high。
fn map_thinking_to_reasoning_effort(body: &Value, chat: &mut Value) {
    if let Some(effort) = body.get("effort").and_then(|e| e.as_str()) {
        let mapped = match effort {
            "max" | "high" => "high",
            "medium" => "medium",
            "low" => "low",
            _ => "high",
        };
        chat["reasoning_effort"] = json!(mapped);
        return;
    }

    let Some(thinking) = body.get("thinking") else {
        return;
    };
    match thinking.get("type").and_then(|t| t.as_str()) {
        Some("enabled") => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|b| b.as_i64())
                .unwrap_or(0);
            let mapped = if budget >= 6000 { "high" } else { "medium" };
            chat["reasoning_effort"] = json!(mapped);
        }
        Some("adaptive") => {
            chat["reasoning_effort"] = json!("high");
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 响应转换：Chat SSE → Anthropic Messages SSE
// ---------------------------------------------------------------------------

struct ToolUseSlot {
    id: String,
    name: String,
    args: String,
    block_idx: usize,
    open: bool,
}

/// 将 OpenAI Chat SSE 流实时转换为 Anthropic Messages SSE 事件流。
pub struct AnthropicStreamConverter {
    msg_id: String,
    pub model: String,
    emitted_start: bool,

    /// 思维链（OpenAI `reasoning_content`）→ Anthropic `thinking` block。
    thinking_content: String,
    thinking_block_open: bool,
    thinking_block_idx: usize,

    text_content: String,
    text_block_open: bool,
    text_block_idx: usize,

    tool_uses: BTreeMap<i64, ToolUseSlot>,
    next_block_idx: usize,

    finish_reason: Option<String>,
    usage: Option<Value>,
}

fn stop_reason_of(finish: Option<&str>) -> &'static str {
    match finish {
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        _ => "end_turn",
    }
}

impl AnthropicStreamConverter {
    pub fn new(model: &str) -> Self {
        Self {
            msg_id: rand_id("msg_"),
            model: model.to_string(),
            emitted_start: false,
            thinking_content: String::new(),
            thinking_block_open: false,
            thinking_block_idx: 0,
            text_content: String::new(),
            text_block_open: false,
            text_block_idx: 0,
            tool_uses: BTreeMap::new(),
            next_block_idx: 0,
            finish_reason: None,
            usage: None,
        }
    }

    /// 处理一行 SSE（如 'data: {...}'），返回 Anthropic SSE 事件字符串。
    pub fn feed_line(&mut self, line: &str) -> String {
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            return String::new();
        }
        let data = line[5..].trim();
        if data == "[DONE]" {
            return String::new();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return String::new();
        };
        self.process_chunk(&chunk)
    }

    /// 流结束，发出收尾事件。
    pub fn finish(&mut self) -> String {
        let mut events: Vec<String> = Vec::new();

        if self.thinking_block_open {
            events.push(Self::evt(
                "content_block_stop",
                &json!({"index": self.thinking_block_idx}),
            ));
            self.thinking_block_open = false;
        }
        if self.text_block_open {
            events.push(Self::evt(
                "content_block_stop",
                &json!({"index": self.text_block_idx}),
            ));
            self.text_block_open = false;
        }
        for tc in self.tool_uses.values_mut() {
            if tc.open {
                events.push(Self::evt(
                    "content_block_stop",
                    &json!({"index": tc.block_idx}),
                ));
                tc.open = false;
            }
        }

        let stop_reason = stop_reason_of(self.finish_reason.as_deref());
        let usage = self.usage.as_ref().map(|u| {
            json!({
                "input_tokens": u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                "output_tokens": u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            })
        });
        let usage_json = usage.unwrap_or(Value::Null);
        events.push(Self::evt(
            "message_delta",
            &json!({
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage_json,
            }),
        ));
        events.push(Self::evt("message_stop", &json!({})));
        events.concat()
    }

    /// 获取完整的非流式 Message 响应对象。
    pub fn get_nonstream_response(&self) -> Value {
        let content = self.build_content_blocks();
        let stop_reason = stop_reason_of(self.finish_reason.as_deref());
        let mut resp = json!({
            "id": self.msg_id,
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": self.model,
            "stop_reason": stop_reason,
            "stop_sequence": null,
        });
        if let Some(u) = &self.usage {
            resp["usage"] = json!({
                "input_tokens": u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                "output_tokens": u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            });
        }
        resp
    }

    fn process_chunk(&mut self, chunk: &Value) -> String {
        let mut events: Vec<String> = Vec::new();

        if let Some(model) = chunk.get("model").and_then(|m| m.as_str()) {
            self.model = model.to_string();
        }

        if !self.emitted_start {
            events.push(Self::evt(
                "message_start",
                &json!({
                    "message": {
                        "id": self.msg_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": self.model,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
            self.emitted_start = true;
        }

        if let Some(usage) = chunk.get("usage") {
            if usage.is_object() && !usage.as_object().unwrap().is_empty() {
                self.usage = Some(usage.clone());
            }
        }

        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return events.concat();
        };
        for choice in choices {
            let delta = choice.get("delta").cloned().unwrap_or(json!({}));
            let finish = choice.get("finish_reason").and_then(|f| f.as_str());

            // reasoning_content delta → Anthropic thinking block
            // （CodeBuddy/腾讯后端把思维链放在 reasoning_content；Anthropic 用 thinking block 承载）
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                if !reasoning.is_empty() {
                    self.thinking_content.push_str(reasoning);
                    if !self.thinking_block_open {
                        self.thinking_block_idx = self.next_block_idx;
                        self.next_block_idx += 1;
                        events.push(Self::evt(
                            "content_block_start",
                            &json!({
                                "index": self.thinking_block_idx,
                                "content_block": {"type": "thinking", "thinking": ""},
                            }),
                        ));
                        self.thinking_block_open = true;
                    }
                    events.push(Self::evt(
                        "content_block_delta",
                        &json!({
                            "index": self.thinking_block_idx,
                            "delta": {"type": "thinking_delta", "thinking": reasoning},
                        }),
                    ));
                }
            }

            // content delta
            let content = delta.get("content").and_then(|c| c.as_str());
            if let Some(content) = content {
                if !content.is_empty() {
                    // 正文开始前，先收尾 thinking block（Anthropic 要求 thinking 在 text 之前）
                    if self.thinking_block_open {
                        events.push(Self::evt(
                            "content_block_stop",
                            &json!({"index": self.thinking_block_idx}),
                        ));
                        self.thinking_block_open = false;
                    }
                    self.text_content.push_str(content);
                    if !self.text_block_open {
                        self.text_block_idx = self.next_block_idx;
                        self.next_block_idx += 1;
                        events.push(Self::evt(
                            "content_block_start",
                            &json!({
                                "index": self.text_block_idx,
                                "content_block": {"type": "text", "text": ""},
                            }),
                        ));
                        self.text_block_open = true;
                    }
                    events.push(Self::evt(
                        "content_block_delta",
                        &json!({
                            "index": self.text_block_idx,
                            "delta": {"type": "text_delta", "text": content},
                        }),
                    ));
                }
            }

            // tool_calls delta
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                    if !self.tool_uses.contains_key(&idx) {
                        let block_idx = self.next_block_idx;
                        self.next_block_idx += 1;
                        self.tool_uses.insert(
                            idx,
                            ToolUseSlot {
                                id: tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                name: String::new(),
                                args: String::new(),
                                block_idx,
                                open: false,
                            },
                        );
                    }
                    let slot = self.tool_uses.get_mut(&idx).unwrap();
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        slot.id = id.to_string();
                    }
                    let function = tc.get("function").cloned().unwrap_or(json!({}));
                    if let Some(name) = function.get("name").and_then(|n| n.as_str()) {
                        slot.name = name.to_string();
                    }

                    if !slot.open {
                        events.push(Self::evt(
                            "content_block_start",
                            &json!({
                                "index": slot.block_idx,
                                "content_block": {"type": "tool_use", "id": slot.id, "name": slot.name, "input": {}},
                            }),
                        ));
                        slot.open = true;
                    }

                    if let Some(args) = function.get("arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            slot.args.push_str(args);
                            events.push(Self::evt(
                                "content_block_delta",
                                &json!({
                                    "index": slot.block_idx,
                                    "delta": {"type": "input_json_delta", "partial_json": args},
                                }),
                            ));
                        }
                    }
                }
            }

            // 仅当 finish_reason 非空时才收尾成块。
            // 部分上游（CodeBuddy/腾讯）每个 chunk 都带 `finish_reason:""`，
            // 若不做非空判断会逐 chunk 反复关闭/重开 block。
            if let Some(finish) = finish {
                if !finish.is_empty() {
                    self.finish_reason = Some(finish.to_string());
                    if self.thinking_block_open {
                        events.push(Self::evt(
                            "content_block_stop",
                            &json!({"index": self.thinking_block_idx}),
                        ));
                        self.thinking_block_open = false;
                    }
                    if self.text_block_open {
                        events.push(Self::evt(
                            "content_block_stop",
                            &json!({"index": self.text_block_idx}),
                        ));
                        self.text_block_open = false;
                    }
                    for tc in self.tool_uses.values_mut() {
                        if tc.open {
                            events.push(Self::evt(
                                "content_block_stop",
                                &json!({"index": tc.block_idx}),
                            ));
                            tc.open = false;
                        }
                    }
                }
            }
        }
        events.concat()
    }

    fn evt(event_type: &str, data: &Value) -> String {
        let mut payload = data.as_object().cloned().unwrap_or_default();
        payload.insert("type".to_string(), json!(event_type));
        format!(
            "event: {event_type}\ndata: {}\n\n",
            Value::Object(payload)
        )
    }

    fn build_content_blocks(&self) -> Value {
        let mut blocks: Vec<Value> = Vec::new();
        if !self.thinking_content.is_empty() || self.thinking_block_open {
            blocks.push(json!({"type": "thinking", "thinking": self.thinking_content}));
        }
        if !self.text_content.is_empty() || self.text_block_open {
            blocks.push(json!({"type": "text", "text": self.text_content}));
        }
        for tc in self.tool_uses.iter() {
            let input: Value = serde_json::from_str(&tc.1.args).unwrap_or(json!(tc.1.args.clone()));
            blocks.push(json!({
                "type": "tool_use",
                "id": tc.1.id,
                "name": tc.1.name,
                "input": input,
            }));
        }
        Value::Array(blocks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_events(text: &str) -> Vec<Value> {
        let mut out = Vec::new();
        for line in text.lines() {
            if let Some(d) = line.strip_prefix("data: ") {
                if let Ok(v) = serde_json::from_str::<Value>(d) {
                    out.push(v);
                }
            }
        }
        out
    }

    #[test]
    fn test_simple_text_request() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "max_tokens": 4096,
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "You are a helpful assistant."}));
        assert_eq!(msgs[1], json!({"role": "user", "content": "Hello"}));
        assert_eq!(chat["model"], "deepseek-v4-pro");
        assert_eq!(chat["max_tokens"], 4096);
    }

    #[test]
    fn test_system_array() {
        let req = json!({
            "model": "auto",
            "max_tokens": 1024,
            "system": [
                {"type": "text", "text": "You are helpful."},
                {"type": "text", "text": "Be concise."},
            ],
            "messages": [],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert_eq!(
            chat["messages"][0]["content"].as_str().unwrap(),
            "You are helpful.\nBe concise."
        );
    }

    #[test]
    fn test_text_and_tool_use() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "max_tokens": 4096,
            "system": "You are a coding assistant.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "List files"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me check."},
                    {"type": "tool_use", "id": "toolu_abc", "name": "Bash", "input": {"cmd": "ls"}},
                ]},
            ],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "Let me check.");
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[2]["tool_calls"][0]["id"], "toolu_abc");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "Bash");
        let args: Value =
            serde_json::from_str(msgs[2]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"cmd": "ls"}));
    }

    #[test]
    fn test_tool_only_no_text() {
        let req = json!({
            "model": "auto",
            "max_tokens": 4096,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "ls"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_123", "name": "Bash", "input": {"cmd": "ls"}},
                ]},
            ],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].is_null());
        assert_eq!(msgs[1]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "toolu_123");
    }

    #[test]
    fn test_tool_result() {
        let req = json!({
            "model": "auto",
            "max_tokens": 4096,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "What files?"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_abc", "name": "Bash", "input": {"cmd": "ls"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_abc", "content": "file1.txt\nfile2.txt"},
                ]},
            ],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "toolu_abc");
        assert_eq!(msgs[2]["content"], "file1.txt\nfile2.txt");
    }

    #[test]
    fn test_tool_result_with_user_text() {
        let req = json!({
            "model": "auto",
            "max_tokens": 4096,
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "Continue."},
                    {"type": "tool_result", "tool_use_id": "toolu_xyz", "content": "output here"},
                ]},
            ],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "toolu_xyz");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Continue.");
    }

    #[test]
    fn test_tools_conversion() {
        let req = json!({
            "model": "deepseek-v4-pro",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "test"}]}],
            "tools": [
                {"name": "Bash", "description": "Run a shell command",
                 "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
            ],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        let tool = &chat["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "Bash");
        assert_eq!(tool["function"]["description"], "Run a shell command");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn test_string_content() {
        let req = json!({
            "model": "auto",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi there"},
            ],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert_eq!(chat["messages"][0], json!({"role": "user", "content": "Hello"}));
        assert_eq!(chat["messages"][1], json!({"role": "assistant", "content": "Hi there"}));
    }

    /// Anthropic thinking/effort → 上游 reasoning_effort 的映射。
    #[test]
    fn test_thinking_maps_to_reasoning_effort() {
        // extended thinking，预算 >= 6000 → high
        let req = json!({
            "model": "m", "max_tokens": 16000,
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert_eq!(chat["reasoning_effort"], "high");

        // 小预算 → medium
        let req = json!({
            "model": "m", "max_tokens": 16000,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert_eq!(chat["reasoning_effort"], "medium");

        // adaptive → high
        let req = json!({
            "model": "m", "max_tokens": 16000,
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert_eq!(chat["reasoning_effort"], "high");

        // 显式 effort=low 优先
        let req = json!({
            "model": "m", "max_tokens": 16000, "effort": "low",
            "thinking": {"type": "enabled", "budget_tokens": 10000},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert_eq!(chat["reasoning_effort"], "low");

        // 不带 thinking/effort → 不注入该字段
        let req = json!({
            "model": "m", "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = anthropic_request_to_chat(&req).unwrap();
        assert!(chat.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_stream_converter_text() {
        let mut conv = AnthropicStreamConverter::new("deepseek-v4-pro");
        let chunks = [
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
            r#"data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
            "data: [DONE]",
        ];
        let mut all_events: Vec<Value> = Vec::new();
        for line in chunks {
            let result = conv.feed_line(line);
            if !result.is_empty() {
                all_events.extend(parse_events(&result));
            }
        }
        all_events.extend(parse_events(&conv.finish()));

        let types: Vec<&str> = all_events
            .iter()
            .filter_map(|e| e.get("type").and_then(|t| t.as_str()))
            .collect();
        for expected in [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(types.contains(&expected), "missing event {expected}: {types:?}");
        }
        // message_start 携带 model
        let start = all_events.iter().find(|e| e["type"] == "message_start").unwrap();
        assert_eq!(start.pointer("/message/model").unwrap(), "deepseek-v4-pro");
        // message_delta 携带 usage 与 stop_reason
        let delta = all_events.iter().find(|e| e["type"] == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["input_tokens"], 5);
        assert_eq!(delta["usage"]["output_tokens"], 2);
        // 非流式响应等价
        let resp = conv.get_nonstream_response();
        assert_eq!(resp["stop_reason"], "end_turn");
        assert_eq!(resp["content"][0]["text"], "Hello world");
    }

    /// reasoning_content → Anthropic thinking block（思维链不得丢失）。
    #[test]
    fn test_stream_converter_reasoning_to_thinking() {
        let mut conv = AnthropicStreamConverter::new("deepseek-v4-flash");
        let chunks = [
            r#"data: {"choices":[{"delta":{"reasoning_content":"Let me"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":" think"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{"content":"391"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ];
        let mut all_events: Vec<Value> = Vec::new();
        for line in chunks {
            let result = conv.feed_line(line);
            if !result.is_empty() {
                all_events.extend(parse_events(&result));
            }
        }
        all_events.extend(parse_events(&conv.finish()));

        // thinking block 应存在，且其 start 在 text block 之前
        let thinking_start = all_events
            .iter()
            .find(|e| {
                e["type"] == "content_block_start"
                    && e.pointer("/content_block/type") == Some(&json!("thinking"))
            })
            .expect("缺少 thinking content_block_start");
        let text_start = all_events
            .iter()
            .find(|e| {
                e["type"] == "content_block_start"
                    && e.pointer("/content_block/type") == Some(&json!("text"))
            })
            .expect("缺少 text content_block_start");
        assert!(thinking_start["index"].as_u64() < text_start["index"].as_u64());

        // thinking_delta 文本累积正确
        let thinking_text: String = all_events
            .iter()
            .filter(|e| {
                e["type"] == "content_block_delta"
                    && e.pointer("/delta/type") == Some(&json!("thinking_delta"))
            })
            .filter_map(|e| e.pointer("/delta/thinking").and_then(|t| t.as_str()))
            .collect();
        assert_eq!(thinking_text, "Let me think");

        // 非流式响应：thinking block 在 text block 之前
        let resp = conv.get_nonstream_response();
        assert_eq!(resp["content"][0]["type"], "thinking");
        assert_eq!(resp["content"][0]["thinking"], "Let me think");
        assert_eq!(resp["content"][1]["type"], "text");
        assert_eq!(resp["content"][1]["text"], "391");
    }

    #[test]
    fn test_stream_converter_tool_use() {
        let mut conv = AnthropicStreamConverter::new("m");
        let chunks = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"Bash","arguments":""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\""}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let mut all_events: Vec<Value> = Vec::new();
        for line in chunks {
            let result = conv.feed_line(line);
            if !result.is_empty() {
                all_events.extend(parse_events(&result));
            }
        }
        all_events.extend(parse_events(&conv.finish()));

        let deltas: Vec<&Value> = all_events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "input_json_delta")
            .collect();
        let partial: String = deltas
            .iter()
            .map(|d| d["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(partial, r#"{"cmd":"ls"}"#);
        let resp = conv.get_nonstream_response();
        assert_eq!(resp["stop_reason"], "tool_use");
        assert_eq!(resp["content"][0]["type"], "tool_use");
        assert_eq!(resp["content"][0]["input"], json!({"cmd": "ls"}));
    }

    #[test]
    fn test_stream_image_request_rejected_for_file_source() {
        let req = json!({
            "model": "auto",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "file", "file_id": "x"}},
            ]}],
        });
        assert!(anthropic_request_to_chat(&req).is_err());
    }
}
