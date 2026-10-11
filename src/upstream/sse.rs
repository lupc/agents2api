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

/// 从 usage 里取「缓存命中的输入 tokens」，字段名按候选链兜底。
///
/// 各上游口径不同：OpenAI 系用 `prompt_tokens_details.cached_tokens`，
/// Anthropic / Buddy 系用 `cache_read_input_tokens` 或 `prompt_cache_hit_tokens`。
/// 全部缺失或为 0 时返回 0（表示「未报告」与「未命中」不可区分，按未命中处理）。
fn cached_tokens_of(usage: &serde_json::Value) -> u64 {
    const CANDIDATES: &[&str] = &[
        "prompt_cache_hit_tokens",
        "cache_read_input_tokens",
        "cached_tokens",
    ];
    let mut best = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    for k in CANDIDATES {
        if let Some(v) = usage.get(*k).and_then(|v| v.as_u64()) {
            best = best.max(v);
        }
    }
    best
}

/// 流式转发时轻量解析统计（finish_reason / tool_calls / usage / 审核拦截）。
#[derive(Default)]
pub struct StreamStats {
    pub finish_reason: Option<String>,
    pub tool_names: Vec<String>,
    pub usage_total: Option<i64>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// 输入中命中上游缓存的 tokens（0 = 上游未报告或未命中）。
    pub cached_tokens: u64,
    pub saw_filter: bool,
    pub buf: Vec<u8>,
    /// 是否已观察到首个内容增量（首个 token 已到达）。
    ///
    /// 用于测量 TTFT：请求发出到第一个 token 返回的耗时。只认「有内容」的
    /// chunk（role / 空 delta 等心跳帧不算），否则 TTFT 会被系统性低估。
    pub saw_token: bool,
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
            // 命中缓存的输入 tokens：用于区分「真实预填充」与「缓存命中」。
            // 各上游字段名不统一（OpenAI 用 prompt_tokens_details.cached_tokens，
            // Anthropic / Buddy 用 cache_read_input_tokens、prompt_cache_hit_tokens），
            // 故按候选链依次兜底，取第一个 >0 的值。
            self.cached_tokens = cached_tokens_of(usage);
        }
        if let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) {
            for ch in choices {
                if let Some(f) = ch.get("finish_reason").and_then(|f| f.as_str()) {
                    if !f.is_empty() {
                        self.finish_reason = Some(f.to_string());
                    }
                }
                // 首个内容增量即视为首 token 到达（content / reasoning / tool_calls 任一）
                let delta = ch.get("delta").unwrap_or(&serde_json::Value::Null);
                let has_content = delta
                    .get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                let has_reasoning = delta
                    .get("reasoning")
                    .or_else(|| delta.get("reasoning_content"))
                    .and_then(|c| c.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                let has_tool = delta
                    .get("tool_calls")
                    .and_then(|t| t.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                if has_content || has_reasoning || has_tool {
                    self.saw_token = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 各上游的缓存字段命名不统一，需按候选链兜底取到值。
    #[test]
    fn test_cached_tokens_field_fallbacks() {
        // OpenAI 系
        assert_eq!(cached_tokens_of(&json!({"prompt_tokens_details": {"cached_tokens": 80}})), 80);
        // Anthropic / Buddy 系
        assert_eq!(cached_tokens_of(&json!({"cache_read_input_tokens": 64})), 64);
        assert_eq!(cached_tokens_of(&json!({"prompt_cache_hit_tokens": 32})), 32);
        assert_eq!(cached_tokens_of(&json!({"cached_tokens": 16})), 16);
        // 缺失 / 为 0 → 0
        assert_eq!(cached_tokens_of(&json!({"prompt_tokens": 100})), 0);
        assert_eq!(cached_tokens_of(&json!({"prompt_cache_hit_tokens": 0})), 0);
    }

    /// 真实上游载荷：命中缓存时 StreamStats 必须拿到 cached_tokens，
    /// 否则预填充速度不会扣除缓存、虚高。
    #[test]
    fn test_stream_stats_reads_cache_from_usage() {
        let mut s = StreamStats::new();
        s.feed(
            br#"data: {"choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":1000,"completion_tokens":50,"cache_read_input_tokens":900}}
"#,
        );
        assert_eq!(s.prompt_tokens, 1000);
        assert_eq!(s.cached_tokens, 900);
    }
}

