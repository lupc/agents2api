//! OpenAI Responses API ↔ Chat Completions API 适配层。
//!
//! Codex CLI 使用 Responses API（POST /v1/responses），而 CodeBuddy 后端只支持
//! Chat Completions 协议。本模块做双向转换：
//!   请求：Responses input/instructions/tools → Chat messages/tools
//!   响应：Chat SSE delta → Responses 语义事件流（response.created / output_text.delta / …）

use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::rand_hex;

fn rand_id(prefix: &str) -> String {
    format!("{prefix}{}", rand_hex(12))
}

// ---------------------------------------------------------------------------
// 请求转换：Responses → Chat
// ---------------------------------------------------------------------------

/// Convert Responses content blocks without dropping images or text order.
fn extract_content(content: &Value) -> Result<Value, String> {
    match content {
        Value::String(s) => Ok(json!(s)),
        Value::Array(parts_in) => {
            let mut parts: Vec<Value> = Vec::new();
            let mut has_image = false;
            for p in parts_in {
                if let Value::String(s) = p {
                    parts.push(json!({"type": "text", "text": s}));
                    continue;
                }
                if !p.is_object() {
                    return Err("Responses content blocks must be text or objects.".to_string());
                }
                let kind = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "input_text" | "text" | "output_text" => {
                        parts.push(json!({
                            "type": "text",
                            "text": p.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                        }));
                    }
                    "input_image" | "image_url" => {
                        let value = p.get("image_url").cloned().unwrap_or(Value::Null);
                        let mut image: serde_json::Map<String, Value> = match &value {
                            Value::String(url) => {
                                let mut m = serde_json::Map::new();
                                m.insert("url".to_string(), json!(url));
                                m
                            }
                            Value::Object(m) => m.clone(),
                            _ => {
                                return Err("Image content requires image_url (URL or data URL); file_id images are not supported.".to_string())
                            }
                        };
                        let url_ok = image
                            .get("url")
                            .and_then(|u| u.as_str())
                            .map(|u| !u.trim().is_empty())
                            .unwrap_or(false);
                        if !url_ok {
                            return Err("Image content requires a non-empty image_url.".to_string());
                        }
                        if let Some(detail) = p.get("detail") {
                            image.insert("detail".to_string(), detail.clone());
                        }
                        parts.push(json!({"type": "image_url", "image_url": image}));
                        has_image = true;
                    }
                    other => {
                        return Err(format!("Unsupported Responses content block: {other:?}"));
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
        Value::Null => Ok(json!("")),
        other => Ok(json!(other.to_string())),
    }
}

/// 将 Responses API 的 input 数组转换为 Chat messages。
#[allow(unused_assignments)]
fn convert_input_items(items: &Value) -> Result<Vec<Value>, String> {
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_assistant_content: Option<Value> = None;
    let mut pending_tool_calls: Vec<Value> = Vec::new();

    macro_rules! flush_assistant {
        () => {
            if pending_assistant_content.is_some() || !pending_tool_calls.is_empty() {
                let mut msg = json!({
                    "role": "assistant",
                    "content": pending_assistant_content.clone().unwrap_or(json!("")),
                });
                if !pending_tool_calls.is_empty() {
                    msg["tool_calls"] = json!(pending_tool_calls.clone());
                }
                messages.push(msg);
                pending_assistant_content = None;
                pending_tool_calls.clear();
            }
        };
    }

    let Some(arr) = items.as_array() else {
        return Ok(messages);
    };

    for item in arr {
        if !item.is_object() {
            continue;
        }
        let item_type = item.get("type").and_then(|t| t.as_str());
        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");

        // 简单消息或 typed message（user/system/developer）
        let is_user_side = matches!(role, "user" | "system" | "developer")
            && (item_type.is_none() || item_type == Some("message"));
        if is_user_side {
            flush_assistant!();
            let mapped_role = if role == "developer" { "system" } else { role };
            let content = extract_content(&item.get("content").cloned().unwrap_or(json!("")))?;
            messages.push(json!({"role": mapped_role, "content": content}));
            continue;
        }

        // assistant 消息（来自前一轮输出）
        if item_type == Some("message") && role == "assistant" {
            flush_assistant!();
            let content_parts = item.get("content").cloned().unwrap_or(json!([]));
            let text = extract_content(&content_parts)?;
            pending_assistant_content = Some(text);
            continue;
        }

        // 简单 role=assistant（无 type 标记）
        if item_type.is_none() && role == "assistant" {
            flush_assistant!();
            let content = extract_content(&item.get("content").cloned().unwrap_or(json!("")))?;
            pending_assistant_content = Some(content);
            continue;
        }

        // function_call — 合并到前面的 assistant 消息
        if item_type == Some("function_call") {
            if pending_assistant_content.is_none() {
                pending_assistant_content = Some(json!(""));
            }
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .cloned()
                .unwrap_or_else(|| json!(rand_id("call_")));
            pending_tool_calls.push(json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                    "arguments": item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                },
            }));
            continue;
        }

        // function_call_output → tool 消息
        if item_type == Some("function_call_output") {
            flush_assistant!();
            let output = extract_content(&item.get("output").cloned().unwrap_or(json!("")))?;
            messages.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
                "content": output,
            }));
            continue;
        }

        // 其他未知类型 — 尝试当作普通消息
        if !role.is_empty() {
            flush_assistant!();
            let content = extract_content(&item.get("content").cloned().unwrap_or(json!("")))?;
            messages.push(json!({"role": role, "content": content}));
        }
    }

    flush_assistant!();
    Ok(messages)
}

/// 将 Responses 格式的 tools 转为 Chat 格式。
fn convert_tools_for_chat(tools: &Value) -> Value {
    let mut result: Vec<Value> = Vec::new();
    if let Value::Array(items) = tools {
        for t in items {
            if !t.is_object() {
                continue;
            }
            if t.get("type").and_then(|ty| ty.as_str()) != Some("function") {
                continue;
            }
            // 已经是 Chat 格式（有 "function" key）
            if t.get("function").is_some() {
                result.push(t.clone());
                continue;
            }
            // Responses 扁平格式 → Chat 嵌套格式
            let mut fn_obj = serde_json::Map::new();
            fn_obj.insert(
                "name".to_string(),
                json!(t.get("name").and_then(|n| n.as_str()).unwrap_or("")),
            );
            if let Some(d) = t.get("description") {
                fn_obj.insert("description".to_string(), d.clone());
            }
            if let Some(p) = t.get("parameters") {
                fn_obj.insert("parameters".to_string(), p.clone());
            }
            if let Some(s) = t.get("strict") {
                fn_obj.insert("strict".to_string(), s.clone());
            }
            result.push(json!({"type": "function", "function": fn_obj}));
        }
    }
    Value::Array(result)
}

/// 将 Responses API 请求体转换为 Chat Completions 请求体。
pub fn responses_request_to_chat(body: &Value) -> Result<Value, String> {
    let mut messages: Vec<Value> = Vec::new();

    // instructions → system message
    if let Some(instructions) = body.get("instructions") {
        if !instructions.is_null() && !instructions.as_str().unwrap_or("").is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    // input → messages
    if let Some(inp) = body.get("input") {
        match inp {
            Value::String(s) => messages.push(json!({"role": "user", "content": s})),
            Value::Array(_) => messages.extend(convert_input_items(inp)?),
            _ => {}
        }
    }

    let mut chat = json!({"messages": messages, "stream": true});

    if let Some(model) = body.get("model") {
        chat["model"] = model.clone();
    }

    if let Some(tools) = body.get("tools") {
        if !tools.is_null() {
            let converted = convert_tools_for_chat(tools);
            if !converted.as_array().unwrap().is_empty() {
                chat["tools"] = converted;
            }
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let mut choice = choice.clone();
        if choice.is_object()
            && choice.get("type").and_then(|t| t.as_str()) == Some("function")
            && choice.get("name").is_some()
        {
            choice = json!({
                "type": "function",
                "function": {"name": choice.get("name").and_then(|n| n.as_str()).unwrap_or("")},
            });
        }
        chat["tool_choice"] = choice;
    }
    if let Some(v) = body.get("parallel_tool_calls") {
        chat["parallel_tool_calls"] = v.clone();
    }

    // 透传常见参数
    for key in [
        "temperature",
        "top_p",
        "stop",
        "seed",
        "presence_penalty",
        "frequency_penalty",
        "response_format",
        "reasoning_effort",
    ] {
        if let Some(v) = body.get(key) {
            chat[key] = v.clone();
        }
    }

    // max_output_tokens → max_tokens
    if let Some(v) = body.get("max_output_tokens") {
        chat["max_tokens"] = v.clone();
    } else if let Some(v) = body.get("max_tokens") {
        chat["max_tokens"] = v.clone();
    }

    Ok(chat)
}

// ---------------------------------------------------------------------------
// 响应转换：Chat → Responses
// ---------------------------------------------------------------------------

struct ToolCallSlot {
    id: String,
    name: String,
    args: String,
    fc_id: String,
    output_idx: usize,
    emitted: bool,
}

/// 将 Chat SSE 流实时转换为 Responses API 语义事件流。
pub struct ResponsesStreamConverter {
    resp_id: String,
    msg_id: String,
    pub model: String,
    created_at: i64,

    emitted_created: bool,
    emitted_msg_item: bool,
    emitted_content_part: bool,

    content: String,
    tool_calls: BTreeMap<i64, ToolCallSlot>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    sequence_number: usize,
    next_output_index: usize,
    msg_output_index: Option<usize>,
    error: Option<Value>,
    finished: bool,
}

fn cached_tokens(usage: &Value) -> i64 {
    let candidates = [
        usage.pointer("/prompt_tokens_details/cached_tokens"),
        usage.pointer("/input_tokens_details/cached_tokens"),
        usage.get("cache_read_input_tokens"),
        usage.get("prompt_cache_hit_tokens"),
        usage.get("cached_tokens"),
    ];
    for v in candidates.into_iter().flatten() {
        if let Some(n) = v.as_i64() {
            if n > 0 {
                return n;
            }
        }
        if let Some(n) = v.as_f64() {
            if n > 0.0 {
                return n as i64;
            }
        }
    }
    0
}

impl ResponsesStreamConverter {
    pub fn new(model: &str) -> Self {
        Self {
            resp_id: rand_id("resp_"),
            msg_id: rand_id("msg_"),
            model: model.to_string(),
            created_at: chrono::Utc::now().timestamp(),
            emitted_created: false,
            emitted_msg_item: false,
            emitted_content_part: false,
            content: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            sequence_number: 0,
            next_output_index: 0,
            msg_output_index: None,
            error: None,
            finished: false,
        }
    }

    /// 处理一行 SSE（如 'data: {...}'），返回转换后的 Responses 事件字符串。
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

    /// 流结束后，发出收尾事件（done + completed）。
    pub fn finish(&mut self) -> String {
        if self.finished || self.error.is_some() {
            return String::new();
        }
        self.finished = true;
        let mut events: Vec<String> = Vec::new();
        let response = self.response_obj("completed");
        let status = response["status"].as_str().unwrap_or("completed").to_string();

        if self.emitted_content_part {
            events.push(Self::evt(
                &mut self.sequence_number,
                "response.output_text.done",
                &json!({
                    "output_index": self.msg_output_index,
                    "item_id": self.msg_id,
                    "content_index": 0,
                    "text": self.content,
                }),
            ));
            events.push(Self::evt(
                &mut self.sequence_number,
                "response.content_part.done",
                &json!({
                    "output_index": self.msg_output_index,
                    "item_id": self.msg_id,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": self.content, "annotations": []},
                }),
            ));
        }

        if self.emitted_msg_item {
            events.push(Self::evt(
                &mut self.sequence_number,
                "response.output_item.done",
                &json!({
                    "output_index": self.msg_output_index,
                    "item": Self::msg_item(&self.msg_id, &self.content, &status, false),
                }),
            ));
        }

        // 关闭 function calls
        for tc in self.tool_calls.values() {
            if tc.emitted {
                events.push(Self::evt(
                    &mut self.sequence_number,
                    "response.function_call_arguments.done",
                    &json!({
                        "output_index": tc.output_idx,
                        "item_id": tc.fc_id,
                        "arguments": tc.args,
                    }),
                ));
                events.push(Self::evt(
                    &mut self.sequence_number,
                    "response.output_item.done",
                    &json!({
                        "output_index": tc.output_idx,
                        "item": Self::fc_item(tc, &status),
                    }),
                ));
            }
        }

        events.push(Self::evt(
            &mut self.sequence_number,
            &format!("response.{status}"),
            &json!({"response": response}),
        ));
        events.concat()
    }

    /// 流结束后获取完整的非流式 Response 对象。
    pub fn get_nonstream_response(&self) -> Value {
        self.response_obj("completed")
    }

    pub fn error(&mut self, message: &str, code: &str) -> String {
        if self.error.is_some() || self.finished {
            return String::new();
        }
        self.error = Some(json!({"message": message, "code": code, "type": "upstream_error"}));
        let resp = self.response_obj("failed");
        Self::evt(
            &mut self.sequence_number,
            "response.failed",
            &json!({"response": resp}),
        )
    }

    fn process_chunk(&mut self, chunk: &Value) -> String {
        let mut events: Vec<String> = Vec::new();
        if self.error.is_some() || self.finished {
            return String::new();
        }
        if let Some(error) = chunk.get("error") {
            if !error.is_null() {
                if let Some(obj) = error.as_object() {
                    let message = obj
                        .get("message")
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| error.to_string());
                    let code = obj
                        .get("code")
                        .map(|c| match c {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_else(|| "upstream_error".to_string());
                    return self.error(&message, &code);
                }
                return self.error(&error.to_string(), "upstream_error");
            }
        }

        if let Some(model) = chunk.get("model").and_then(|m| m.as_str()) {
            self.model = model.to_string();
        }

        if !self.emitted_created {
            let resp = self.response_obj("in_progress");
            events.push(Self::evt(
                &mut self.sequence_number,
                "response.created",
                &json!({"response": resp.clone()}),
            ));
            events.push(Self::evt(
                &mut self.sequence_number,
                "response.in_progress",
                &json!({"response": resp}),
            ));
            self.emitted_created = true;
        }

        if let Some(usage) = chunk.get("usage") {
            if usage.is_object() && !usage.as_object().unwrap().is_empty() {
                let merged = match self.usage.take() {
                    Some(mut prev) => {
                        if let (Some(pm), Some(nm)) =
                            (prev.as_object_mut(), usage.as_object())
                        {
                            for (k, v) in nm {
                                match (pm.get_mut(k), v) {
                                    (Some(Value::Object(old)), Value::Object(new)) => {
                                        for (ik, iv) in new {
                                            old.insert(ik.clone(), iv.clone());
                                        }
                                    }
                                    (_, Value::Null) => {}
                                    (_, _) => {
                                        pm.insert(k.clone(), v.clone());
                                    }
                                }
                            }
                        }
                        prev
                    }
                    None => usage.clone(),
                };
                self.usage = Some(merged);
            }
        }

        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return events.concat();
        };
        for choice in choices {
            let delta = choice.get("delta").cloned().unwrap_or(json!({}));
            let finish = choice.get("finish_reason").and_then(|f| f.as_str());

            // content delta
            let content = delta.get("content").and_then(|c| c.as_str());
            if let Some(content) = content {
                if !content.is_empty() {
                    if !self.emitted_msg_item {
                        self.msg_output_index = Some(self.next_output_index);
                        self.next_output_index += 1;
                        events.push(Self::evt(
                            &mut self.sequence_number,
                            "response.output_item.added",
                            &json!({
                                "output_index": self.msg_output_index,
                                "item": Self::msg_item(&self.msg_id, "", "in_progress", true),
                            }),
                        ));
                        self.emitted_msg_item = true;
                    }
                    if !self.emitted_content_part {
                        events.push(Self::evt(
                            &mut self.sequence_number,
                            "response.content_part.added",
                            &json!({
                                "output_index": self.msg_output_index,
                                "item_id": self.msg_id,
                                "content_index": 0,
                                "part": {"type": "output_text", "text": "", "annotations": []},
                            }),
                        ));
                        self.emitted_content_part = true;
                    }
                    self.content.push_str(content);
                    events.push(Self::evt(
                        &mut self.sequence_number,
                        "response.output_text.delta",
                        &json!({
                            "output_index": self.msg_output_index,
                            "item_id": self.msg_id,
                            "content_index": 0,
                            "delta": content,
                        }),
                    ));
                }
            }

            // tool_calls delta
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                    if !self.tool_calls.contains_key(&idx) {
                        let oi = self.next_output_index;
                        self.next_output_index += 1;
                        self.tool_calls.insert(
                            idx,
                            ToolCallSlot {
                                id: tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                name: String::new(),
                                args: String::new(),
                                fc_id: rand_id("fc_"),
                                output_idx: oi,
                                emitted: false,
                            },
                        );
                    }
                    let slot = self.tool_calls.get_mut(&idx).unwrap();
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        slot.id = id.to_string();
                    }
                    let function = tc.get("function").cloned().unwrap_or(json!({}));
                    if let Some(name) = function.get("name").and_then(|n| n.as_str()) {
                        slot.name = name.to_string();
                    }

                    if !slot.emitted {
                        events.push(Self::evt(
                            &mut self.sequence_number,
                            "response.output_item.added",
                            &json!({
                                "output_index": slot.output_idx,
                                "item": Self::fc_item(slot, "in_progress"),
                            }),
                        ));
                        slot.emitted = true;
                    }

                    if let Some(args) = function.get("arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            slot.args.push_str(args);
                            events.push(Self::evt(
                                &mut self.sequence_number,
                                "response.function_call_arguments.delta",
                                &json!({
                                    "output_index": slot.output_idx,
                                    "item_id": slot.fc_id,
                                    "delta": args,
                                }),
                            ));
                        }
                    }
                }
            }

            if let Some(finish) = finish {
                self.finish_reason = Some(finish.to_string());
            }
        }
        events.concat()
    }

    fn evt(seq: &mut usize, event_type: &str, data: &Value) -> String {
        let mut payload = data.as_object().cloned().unwrap_or_default();
        payload.insert("type".to_string(), json!(event_type));
        payload.insert("sequence_number".to_string(), json!(seq));
        *seq += 1;
        format!(
            "event: {event_type}\ndata: {}\n\n",
            Value::Object(payload)
        )
    }

    fn msg_item(msg_id: &str, content: &str, status: &str, empty: bool) -> Value {
        let content = if empty {
            json!([])
        } else {
            json!([{"type": "output_text", "text": content, "annotations": []}])
        };
        json!({
            "type": "message",
            "id": msg_id,
            "status": status,
            "role": "assistant",
            "content": content,
        })
    }

    fn fc_item(tc: &ToolCallSlot, status: &str) -> Value {
        json!({
            "type": "function_call",
            "id": tc.fc_id,
            "call_id": tc.id,
            "name": tc.name,
            "arguments": tc.args,
            "status": status,
        })
    }

    fn response_obj(&self, status: &str) -> Value {
        let mut status = status.to_string();
        let mut incomplete: Value = Value::Null;
        if self.error.is_some() {
            status = "failed".to_string();
        } else if status == "completed"
            && matches!(
                self.finish_reason.as_deref(),
                Some("length") | Some("content_filter") | Some("content-filter")
            )
        {
            status = "incomplete".to_string();
            let reason = if self.finish_reason.as_deref() == Some("length") {
                "max_output_tokens"
            } else {
                "content_filter"
            };
            incomplete = json!({"reason": reason});
        }

        // 收集 output 并按 output index 排序
        let mut indexed: Vec<(usize, Value)> = Vec::new();
        if self.emitted_msg_item || !self.content.is_empty() {
            let idx = self.msg_output_index.unwrap_or(0);
            indexed.push((idx, Self::msg_item(&self.msg_id, &self.content, &status, false)));
        }
        let mut index_map: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for tc in self.tool_calls.values() {
            if tc.emitted {
                indexed.push((tc.output_idx, Self::fc_item(tc, &status)));
            }
            index_map.insert(tc.fc_id.clone(), tc.output_idx);
        }
        index_map.insert(self.msg_id.clone(), self.msg_output_index.unwrap_or(0));
        indexed.sort_by_key(|(idx, _)| *idx);
        let output: Vec<Value> = indexed.into_iter().map(|(_, item)| item).collect();

        let usage = self.usage.as_ref().map(|u| {
            let as_i64 = |v: Option<&Value>| v.and_then(|x| x.as_i64());
            // 与 Python .get(key, default) 一致：键存在即取值（含 0），否则回退
            let prompt = as_i64(u.get("prompt_tokens"))
                .unwrap_or_else(|| as_i64(u.get("input_tokens")).unwrap_or(0));
            let completion = as_i64(u.get("completion_tokens"))
                .unwrap_or_else(|| as_i64(u.get("output_tokens")).unwrap_or(0));
            json!({
                "input_tokens": prompt,
                "input_tokens_details": {"cached_tokens": cached_tokens(u)},
                "output_tokens": completion,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": as_i64(u.get("total_tokens")).unwrap_or(prompt + completion),
            })
        });

        json!({
            "id": self.resp_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "error": self.error.clone(),
            "incomplete_details": incomplete,
            "model": self.model,
            "output": output,
            "parallel_tool_calls": true,
            "usage": usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(conv: &mut ResponsesStreamConverter, obj: &Value) -> String {
        conv.feed_line(&format!("data: {obj}"))
    }

    fn events(text: &str) -> Vec<Value> {
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|d| serde_json::from_str::<Value>(d).unwrap())
            .collect()
    }

    #[test]
    fn test_simple_text_request() {
        let req = json!({
            "model": "glm-5.2",
            "input": "Hello, how are you?",
            "instructions": "You are a helpful assistant.",
            "stream": true,
        });
        let chat = responses_request_to_chat(&req).unwrap();
        assert_eq!(
            chat["messages"][0],
            json!({"role": "system", "content": "You are a helpful assistant."})
        );
        assert_eq!(
            chat["messages"][1],
            json!({"role": "user", "content": "Hello, how are you?"})
        );
        assert_eq!(chat["model"], "glm-5.2");
    }

    #[test]
    fn test_array_input_request() {
        let req = json!({
            "model": "glm-5.2",
            "input": [
                {"role": "user", "content": "Fix the bug"},
                {"type": "message", "id": "msg_1", "role": "assistant",
                 "content": [{"type": "output_text", "text": "I'll check the file."}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_123",
                 "name": "shell", "arguments": "{\"cmd\":\"cat main.py\"}"},
                {"type": "function_call_output", "call_id": "call_123", "output": "print('hello')"},
                {"role": "user", "content": "Now fix it"},
            ],
            "instructions": "You are a coding assistant.",
        });
        let chat = responses_request_to_chat(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0], json!({"role": "system", "content": "You are a coding assistant."}));
        assert_eq!(msgs[1], json!({"role": "user", "content": "Fix the bug"}));
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "I'll check the file.");
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_123");
        assert_eq!(msgs[4], json!({"role": "user", "content": "Now fix it"}));
    }

    #[test]
    fn test_tools_conversion() {
        let req = json!({
            "model": "glm-5.2",
            "input": "test",
            "tools": [
                {"type": "function", "name": "shell", "description": "Run a shell command",
                 "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
            ],
        });
        let chat = responses_request_to_chat(&req).unwrap();
        let tool = &chat["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "shell");
    }

    #[test]
    fn test_max_output_tokens() {
        let req = json!({"model": "glm-5.2", "input": "test", "max_output_tokens": 4096});
        let chat = responses_request_to_chat(&req).unwrap();
        assert_eq!(chat["max_tokens"], 4096);
    }

    #[test]
    fn test_developer_role() {
        let req = json!({"model": "glm-5.2", "input": [
            {"role": "developer", "content": "Be concise."},
            {"role": "user", "content": "Hi"},
        ]});
        let chat = responses_request_to_chat(&req).unwrap();
        assert_eq!(chat["messages"][0], json!({"role": "system", "content": "Be concise."}));
        assert_eq!(chat["messages"][1], json!({"role": "user", "content": "Hi"}));
    }

    #[test]
    fn test_named_tool_choice() {
        let req = json!({"input": "hi", "parallel_tool_calls": false,
                         "tool_choice": {"type": "function", "name": "read"}});
        let chat = responses_request_to_chat(&req).unwrap();
        assert_eq!(chat["tool_choice"], json!({"type": "function", "function": {"name": "read"}}));
        assert_eq!(chat["parallel_tool_calls"], false);
    }

    #[test]
    fn test_cache_variants_and_partial_usage() {
        let caches = [
            json!({"prompt_tokens_details": {"cached_tokens": 80}}),
            json!({"input_tokens_details": {"cached_tokens": 80}}),
            json!({"cache_read_input_tokens": 80}),
            json!({"prompt_tokens_details": {"cached_tokens": 0}, "prompt_cache_hit_tokens": 80}),
            json!({"cached_tokens": 80}),
        ];
        for cache in caches {
            let mut conv = ResponsesStreamConverter::new("m");
            let mut usage = json!({"prompt_tokens": 100});
            let obj = usage.as_object_mut().unwrap();
            for (k, v) in cache.as_object().unwrap() {
                obj.insert(k.clone(), v.clone());
            }
            feed(&mut conv, &json!({"choices": [], "usage": usage}));
            feed(
                &mut conv,
                &json!({"choices": [], "usage": {"completion_tokens": 10, "prompt_tokens_details": {"audio_tokens": 0}}}),
            );
            let usage = conv.get_nonstream_response()["usage"].clone();
            assert_eq!(usage["input_tokens"], 100);
            assert_eq!(usage["input_tokens_details"]["cached_tokens"], 80);
            assert_eq!(usage["output_tokens"], 10);
            assert_eq!(usage["total_tokens"], 110);
        }
    }

    #[test]
    fn test_interleaved_tools_and_text_have_stable_indices() {
        let mut conv = ResponsesStreamConverter::new("m");
        let mut result = String::new();
        for delta in [
            json!({"tool_calls": [{"index": 0, "id": "call1", "function": {"name": "read", "arguments": "{"}}]}),
            json!({"content": "Reading"}),
            json!({"tool_calls": [
                {"index": 0, "function": {"arguments": "}"}},
                {"index": 1, "id": "call2", "function": {"name": "write", "arguments": "{}"}},
            ]}),
        ] {
            result += &feed(&mut conv, &json!({"choices": [{"delta": delta}]}));
        }
        result += &conv.finish();
        let parsed = events(&result);
        let added: Vec<&Value> = parsed
            .iter()
            .filter(|e| e["type"] == "response.output_item.added")
            .collect();
        let indices: Vec<i64> = added
            .iter()
            .map(|e| e["output_index"].as_i64().unwrap())
            .collect();
        assert_eq!(indices, vec![0, 1, 2]);
        let ids: std::collections::HashMap<i64, String> = added
            .iter()
            .map(|e| {
                (
                    e["output_index"].as_i64().unwrap(),
                    e["item"]["id"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        for event in &parsed {
            if let Some(item_id) = event.get("item_id").and_then(|v| v.as_str()) {
                let oi = event["output_index"].as_i64().unwrap();
                assert_eq!(item_id, ids[&oi], "event {}", event["type"]);
            }
        }
        let final_output = &parsed.last().unwrap()["response"]["output"];
        let final_ids: Vec<&str> = final_output
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = [0i64, 1, 2]
            .iter()
            .map(|i| ids[i].as_str())
            .collect();
        assert_eq!(final_ids, expected);
        let seqs: Vec<usize> = parsed
            .iter()
            .map(|e| e["sequence_number"].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(seqs, (0..parsed.len()).collect::<Vec<_>>());
        assert_eq!(conv.finish(), "");
    }

    #[test]
    fn test_upstream_error_cannot_become_empty_success() {
        let mut conv = ResponsesStreamConverter::new("m");
        let result = feed(&mut conv, &json!({"error": {"message": "limited", "code": 429}}));
        assert_eq!(events(&result)[0]["type"], "response.failed");
        assert_eq!(conv.finish(), "");
        assert_eq!(conv.get_nonstream_response()["status"], "failed");
    }

    #[test]
    fn test_incomplete_output() {
        for (reason, expected) in [("length", "max_output_tokens"), ("content_filter", "content_filter")] {
            let mut conv = ResponsesStreamConverter::new("m");
            feed(
                &mut conv,
                &json!({"choices": [{"delta": {"content": "partial"}, "finish_reason": reason}]}),
            );
            let event = events(&conv.finish()).last().unwrap().clone();
            assert_eq!(event["type"], "response.incomplete");
            assert_eq!(event["response"]["status"], "incomplete");
            assert_eq!(event["response"]["incomplete_details"]["reason"], expected);
        }
    }
}