//! 上游 SSE 处理：非流式聚合、错误事件构造。

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// 解析一行 SSE 的 data 载荷；非 data 行 / [DONE] / 非法 JSON 返回 None。
pub fn parse_data_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with("data:") {
        return None;
    }
    let data = line[5..].trim();
    if data == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(data).ok()
}

/// 以 OpenAI SSE 错误 chunk 形式返回的事件。
pub fn err_event(msg: &str, status: u16) -> String {
    let chunk = json!({
        "error": {
            "message": msg.chars().take(500).collect::<String>(),
            "type": "upstream_error",
            "code": status,
        }
    });
    format!("data: {chunk}\n\n")
}

/// 内容审核拦截特征（finish_reason 或中文文案）。
pub fn looks_like_content_filter_text(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("content-filter")
        || text.contains("content_filter")
        || text.contains("敏感内容")
        || text.contains("内容审核")
        || text.contains("无法响应您的请求")
}

/// 消费后端的 OpenAI SSE 流，聚合成单个非流式 chat.completion 对象。
pub struct ChatAggregator {
    content_parts: Vec<String>,
    tool_calls: BTreeMap<i64, (Option<String>, Option<String>, String)>,
    pub model: Option<String>,
    pub finish_reason: Option<String>,
    pub usage: Option<Value>,
}

impl ChatAggregator {
    pub fn new() -> Self {
        Self {
            content_parts: Vec::new(),
            tool_calls: BTreeMap::new(),
            model: None,
            finish_reason: None,
            usage: None,
        }
    }

    pub fn feed_line(&mut self, line: &str) {
        let Some(chunk) = parse_data_line(line) else {
            return;
        };
        if let Some(model) = chunk.get("model").and_then(|m| m.as_str()) {
            self.model = Some(model.to_string());
        }
        if let Some(usage) = chunk.get("usage") {
            if usage.is_object() && !usage.as_object().unwrap().is_empty() {
                self.usage = Some(usage.clone());
            }
        }
        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            return;
        };
        for choice in choices {
            if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                if !finish.is_empty() {
                    self.finish_reason = Some(finish.to_string());
                }
            }
            let delta = choice.get("delta").cloned().unwrap_or(json!({}));
            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    self.content_parts.push(content.to_string());
                }
            }
            if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                    let slot = self
                        .tool_calls
                        .entry(idx)
                        .or_insert_with(|| (None, None, String::new()));
                    // id / name 只在非空时更新：上游可能把一次 tool_call 拆成多个增量块，
                    // 后续块带空值，直接覆盖会丢掉首块已建立的 id/name。
                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        if !id.is_empty() {
                            slot.0 = Some(id.to_string());
                        }
                    }
                    if let Some(name) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                    {
                        if !name.is_empty() {
                            slot.1 = Some(name.to_string());
                        }
                    }
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|a| a.as_str())
                    {
                        slot.2.push_str(args);
                    }
                }
            }
        }
    }

    pub fn finish(self) -> Value {
        let mut tcs: Option<Vec<Value>> = None;
        if !self.tool_calls.is_empty() {
            let mut list = Vec::new();
            for (_, (id, name, arguments)) in self.tool_calls {
                list.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                }));
            }
            tcs = Some(list);
        }
        // 有 tool_calls 时 finish_reason 必须是 tool_calls：
        // 部分上游（如 Trae）在有工具调用时仍返回 "stop"，会误导客户端不执行工具。
        let finish_reason = if tcs.is_some() {
            Some("tool_calls".to_string())
        } else {
            self.finish_reason.clone()
        };

        let mut message = json!({
            "role": "assistant",
            "content": if self.content_parts.is_empty() { Value::Null } else { json!(self.content_parts.concat()) },
        });
        if let Some(tcs) = &tcs {
            message["tool_calls"] = json!(tcs);
        }
        json!({
            "id": format!("chatcmpl-{}", crate::rand_hex(12)),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": self.model.unwrap_or_else(|| "unknown".to_string()),
            "choices": [
                {"index": 0, "message": message, "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string())}
            ],
            "usage": self.usage.unwrap_or_else(|| json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})),
        })
    }
}

/// 流式转发时轻量解析统计（finish_reason / tool_calls / usage / 审核拦截）。
#[derive(Default)]
pub struct StreamStats {
    pub finish_reason: Option<String>,
    pub tool_names: Vec<String>,
    pub usage_total: Option<i64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub saw_filter: bool,
    pub buf: Vec<u8>,
}

impl StreamStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&c| c == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let mut line = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
            if line.ends_with('\r') {
                line.pop();
            }
            self.feed_line(&line);
        }
    }

    fn feed_line(&mut self, line: &str) {
        let Some(obj) = parse_data_line(line) else {
            return;
        };
        if let Some(usage) = obj.get("usage") {
            if let Some(t) = usage.get("total_tokens").and_then(|v| v.as_i64()) {
                self.usage_total = Some(t);
            }
            if let Some(p) = usage
                .get("prompt_tokens")
                .or_else(|| usage.get("input_tokens"))
                .and_then(|v| v.as_u64())
            {
                self.prompt_tokens = p;
            }
            if let Some(c) = usage
                .get("completion_tokens")
                .or_else(|| usage.get("output_tokens"))
                .and_then(|v| v.as_u64())
            {
                self.completion_tokens = c;
            }
        }
        if let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) {
            for ch in choices {
                if let Some(f) = ch.get("finish_reason").and_then(|f| f.as_str()) {
                    if !f.is_empty() {
                        self.finish_reason = Some(f.to_string());
                    }
                }
                if let Some(tcs) = ch
                    .pointer("/delta/tool_calls")
                    .and_then(|t| t.as_array())
                {
                    for tc in tcs {
                        if let Some(nm) = tc
                            .pointer("/function/name")
                            .and_then(|n| n.as_str())
                        {
                            if !nm.is_empty() {
                                self.tool_names.push(nm.to_string());
                            }
                        }
                    }
                }
            }
        }
        if line.contains("content-filter") || line.contains("敏感") || line.contains("审核") {
            self.saw_filter = true;
        }
    }
}

