//! `/v1/responses` 的后端投影层。
//!
//! Codex CLI 会把大量运行时提示、完整工具 schema、长历史、以及工具输出一并塞进
//! /v1/responses 请求里。腾讯后端对这类 agentic payload 很容易触发内容审核，或者
//! 因为上下文过长而表现不稳定。
//!
//! 本模块在保持外部 OpenAI Responses 兼容的前提下，只对发往后端的 Chat body 做
//! "最小语义闭包"投影（仅当 `CODEBUDDY_LOSSY_PROJECTION=1` 时启用）。

use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};

const AGENTIC_TOOL_NAMES: &[&str] = &[
    "exec_command",
    "write_stdin",
    "update_plan",
    "request_user_input",
    "view_image",
    "get_goal",
    "create_goal",
    "update_goal",
    "apply_patch",
    "tool_search_tool",
];

const HARNESS_USER_MARKERS: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<permissions instructions>",
    "<collaboration_mode>",
    "<skills_instructions>",
    "<system-reminder>",
    "# claudeMd",
];

const HARNESS_SYSTEM_MARKERS: &[&str] = &[
    "You are a coding agent running in the Codex CLI",
    "Within this context, Codex refers to",
    "# AGENTS.md spec",
    "<permissions instructions>",
    "<collaboration_mode>",
    "<skills_instructions>",
    "The following deferred tools are now available via ToolSearch.",
    "### Available skills",
    "## request_user_input availability",
    "You are Claude Code",
];

const BASE_SYSTEM_PROMPT: &str = "You are a coding assistant serving an OpenAI-compatible CLI. \
Be precise, concise, safe, and action-oriented. \
Use available tools when needed, follow repository instructions and durable user context, \
and continue from the preserved recent context. \
If earlier history was condensed, rely on the preserved recent messages and rerun tools when exact old details are required.";

const HISTORY_PREFIX: &str = "Earlier conversation summary (condensed):";

const MAX_SYSTEM_GUIDANCE_CHARS: usize = 1200;
const MAX_USER_CHARS: usize = 3200;
const MAX_ASSISTANT_CHARS: usize = 1800;
const MAX_TOOL_OUTPUT_CHARS: usize = 1600;
const MAX_TOOL_ARGS_CHARS: usize = 900;
const MAX_HISTORY_SUMMARY_CHARS: usize = 2200;
const MAX_HISTORY_ITEMS: usize = 10;
const MAX_TAIL_MESSAGES: usize = 8;
const MAX_TAIL_CHARS: usize = 7000;

const SCHEMA_KEEP_KEYS: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "oneOf",
    "anyOf",
    "allOf",
    "additionalProperties",
    "format",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "nullable",
];

// ---------------------------------------------------------------------------
// 文本工具
// ---------------------------------------------------------------------------

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Python `_truncate_text`：按字符数截断并追加提示。
fn truncate_text(s: &str, limit: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let n = chars(s);
    if n <= limit {
        return s.to_string();
    }
    let keep = limit.saturating_sub(24);
    let head: String = s.chars().take(keep).collect();
    format!(
        "{} ... [truncated {} chars]",
        head.trim_end(),
        n - keep
    )
}

/// Python `_summarize_free_text`：保留头 1/2 与尾 1/3。
fn summarize_free_text(s: &str, limit: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    let n = chars(s);
    if n <= limit {
        return s.to_string();
    }
    let head_n = limit / 2;
    let tail_n = limit / 3;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = {
        let skip = n.saturating_sub(tail_n);
        s.chars().skip(skip).collect()
    };
    let omitted = n - head_n - tail_n;
    format!("{head}\n... [{omitted} chars omitted] ...\n{tail}")
}

/// 把 content（字符串或 blocks）规整成纯文本。
fn content_to_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                match block {
                    Value::String(s) => parts.push(s.clone()),
                    Value::Object(_) => {
                        if let Some(t) = block.get("text") {
                            parts.push(value_to_plain_string(t));
                        } else if let Some(o) = block.get("output") {
                            parts.push(value_to_plain_string(o));
                        }
                    }
                    _ => {}
                }
            }
            parts.concat()
        }
        other => value_to_plain_string(other),
    }
}

fn value_to_plain_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

fn looks_like_harness_user(text: &str) -> bool {
    HARNESS_USER_MARKERS.iter().any(|m| text.contains(m))
}

fn looks_like_harness_system(text: &str) -> bool {
    HARNESS_SYSTEM_MARKERS.iter().any(|m| text.contains(m))
}

// ---------------------------------------------------------------------------
// 消息投影
// ---------------------------------------------------------------------------

fn message_cost(msg: &Value) -> usize {
    let mut cost = chars(&content_to_text(msg.get("content").unwrap_or(&Value::Null)));
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let f = tc.get("function");
            cost += chars(f
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or(""));
            cost += chars(f
                .and_then(|f| f.get("arguments"))
                .map(value_to_plain_string)
                .unwrap_or_default()
                .as_str());
        }
    }
    cost
}

fn messages_size(messages: &[Value]) -> usize {
    let mut total = 0;
    for msg in messages {
        if !msg.is_object() {
            continue;
        }
        total += message_cost(msg);
        total += chars(msg.get("role").and_then(|r| r.as_str()).unwrap_or(""));
    }
    total
}

fn tools_size(tools: &Value) -> usize {
    tools.to_string().len()
}

fn tool_name(tool: &Value) -> String {
    if !tool.is_object() {
        return String::new();
    }
    let function = tool.get("function").unwrap_or(tool);
    function
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string()
}

/// 超长 tool output 摘要（保头 10 行 + 尾 6 行）。
fn summarize_tool_output(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if chars(text) <= MAX_TOOL_OUTPUT_CHARS && text.matches('\n').count() <= 24 {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let exit_line = lines
        .iter()
        .find(|l| l.contains("Process exited with code"))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    let mut useful_lines: Vec<String> = Vec::new();
    for line in &lines {
        let stripped = line.trim_end();
        if stripped.trim() == "Output:" {
            continue;
        }
        if stripped.starts_with("Chunk ID:")
            || stripped.starts_with("Wall time:")
            || stripped.starts_with("Original token count:")
            || stripped.starts_with("Process exited with code")
        {
            continue;
        }
        useful_lines.push(stripped.to_string());
    }
    let body_lines = useful_lines;

    let head: Vec<String> = body_lines.iter().take(10).cloned().collect();
    let tail: Vec<String> = if body_lines.len() > 16 {
        body_lines
            .iter()
            .skip(body_lines.len().saturating_sub(6))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let omitted = body_lines
        .len()
        .saturating_sub(head.len() + tail.len());

    let mut parts: Vec<String> = Vec::new();
    if !exit_line.is_empty() {
        parts.push(exit_line);
    }
    if !head.is_empty() {
        parts.push("Key output:".to_string());
        parts.extend(head);
    }
    if omitted > 0 {
        parts.push(format!("... [omitted {omitted} lines] ..."));
    }
    if !tail.is_empty() {
        parts.push("Recent tail:".to_string());
        parts.extend(tail);
    }

    let summary = parts.join("\n").trim().to_string();
    if summary.is_empty() {
        truncate_text(text, MAX_TOOL_OUTPUT_CHARS)
    } else {
        truncate_text(&summary, MAX_TOOL_OUTPUT_CHARS)
    }
}

fn tool_output_inline_summary(text: &str) -> String {
    let summarized = summarize_tool_output(text).replace('\n', " | ");
    truncate_text(&summarized, 220)
}

fn tool_call_name_map(messages: &[Value]) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    for msg in messages {
        if !msg.is_object() || msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let call_id = tc.get("id").and_then(|v| v.as_str());
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str());
                if let (Some(call_id), Some(name)) = (call_id, name) {
                    mapping.insert(call_id.to_string(), name.to_string());
                }
            }
        }
    }
    mapping
}

/// 历史摘要（逐条压缩为单行）。
fn history_summary(messages: &[Value], names: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut total_chars = 0usize;
    let mut summarized = 0usize;

    for msg in messages {
        let line = history_line(msg, names);
        if line.is_empty() {
            continue;
        }
        if summarized >= MAX_HISTORY_ITEMS || total_chars + chars(&line) > MAX_HISTORY_SUMMARY_CHARS
        {
            break;
        }
        lines.push(format!("- {line}"));
        total_chars += chars(&line);
        summarized += 1;
    }

    let remaining = messages.len().saturating_sub(summarized);
    if remaining > 0 {
        lines.push(format!(
            "- {remaining} earlier messages or tool results were further condensed."
        ));
    }

    if lines.is_empty() {
        return String::new();
    }
    format!("{HISTORY_PREFIX}\n{}", lines.join("\n"))
}

fn history_line(msg: &Value, names: &HashMap<String, String>) -> String {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));

    match role {
        "user" => format!("User asked: {}", truncate_text(&text, 220)),
        "assistant" => {
            let mut tool_names: Vec<String> = Vec::new();
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    if let Some(n) = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                    {
                        if !n.is_empty() {
                            tool_names.push(n.to_string());
                        }
                    }
                }
            }
            if !text.is_empty() && !tool_names.is_empty() {
                format!(
                    "Assistant replied: {} Then called tools: {}.",
                    truncate_text(&text, 160),
                    tool_names.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
                )
            } else if !tool_names.is_empty() {
                format!(
                    "Assistant called tools: {}.",
                    tool_names.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
                )
            } else if !text.is_empty() {
                format!("Assistant replied: {}", truncate_text(&text, 180))
            } else {
                String::new()
            }
        }
        "tool" => {
            let call_id = msg.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
            let tool_name = names.get(call_id).cloned().unwrap_or_else(|| "tool".to_string());
            format!("Tool {tool_name} returned: {}", tool_output_inline_summary(&text))
        }
        "system" => format!("System guidance: {}", truncate_text(&text, 180)),
        _ => String::new(),
    }
}

fn merge_guidance_messages(messages: &[String]) -> String {
    let mut merged: Vec<String> = Vec::new();
    let mut total = 0usize;
    for message in messages.iter().take(2) {
        let text = message.trim();
        if text.is_empty() {
            continue;
        }
        let mut text = text.to_string();
        if total + chars(&text) > MAX_SYSTEM_GUIDANCE_CHARS {
            text = truncate_text(&text, MAX_SYSTEM_GUIDANCE_CHARS.saturating_sub(total));
        }
        total += chars(&text);
        merged.push(text);
        if total >= MAX_SYSTEM_GUIDANCE_CHARS {
            break;
        }
    }
    if merged.is_empty() {
        return String::new();
    }
    if merged.len() == 1 {
        return merged[0].clone();
    }
    format!("Additional instructions:\n{}", merged.join("\n\n"))
}

fn summarize_tool_arguments(name: &str, arguments: &Value) -> String {
    match arguments {
        Value::String(arguments) => {
            if chars(arguments) <= MAX_TOOL_ARGS_CHARS {
                return arguments.clone();
            }
            if name == "apply_patch" {
                return json!({"summary": "Large apply_patch payload omitted; a patch was prepared or applied in a previous step."})
                    .to_string();
            }
            match serde_json::from_str::<Value>(arguments) {
                Ok(parsed) => json!(shrink_json_value(&parsed, 0, "")).to_string(),
                Err(_) => json!({"summary": truncate_text(arguments, 320)}).to_string(),
            }
        }
        // 非字符串参数：直接序列化（与上游一致，不做收缩）
        other => other.to_string(),
    }
}

fn shrink_json_value(value: &Value, depth: usize, key: &str) -> Value {
    if depth >= 4 {
        return json!("<omitted>");
    }
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (idx, (k, v)) in map.iter().enumerate() {
                if idx >= 12 {
                    out.insert("_omitted_keys".to_string(), json!(map.len() - idx));
                    break;
                }
                out.insert(k.clone(), shrink_json_value(v, depth + 1, k));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut trimmed: Vec<Value> = items
                .iter()
                .take(6)
                .map(|i| shrink_json_value(i, depth + 1, key))
                .collect();
            if items.len() > 6 {
                trimmed.push(json!(format!("<omitted {} items>", items.len() - 6)));
            }
            Value::Array(trimmed)
        }
        Value::String(s) => {
            let limit = if ["cmd", "chars", "patch", "content", "text", "question"].contains(&key) {
                240
            } else {
                120
            };
            json!(truncate_text(s, limit))
        }
        _ => value.clone(),
    }
}

fn project_tool_call(tool_call: &Value) -> Option<Value> {
    if !tool_call.is_object() {
        return None;
    }
    let function = tool_call.get("function").unwrap_or(&Value::Null);
    let name = function
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let arguments = function.get("arguments").cloned().unwrap_or(json!(""));
    Some(json!({
        "id": tool_call.get("id").cloned().unwrap_or(Value::Null),
        "type": tool_call.get("type").cloned().unwrap_or(json!("function")),
        "function": {
            "name": name,
            "arguments": summarize_tool_arguments(&name, &arguments),
        }
    }))
}

fn project_conversation_message(msg: &Value, conservative: bool) -> Option<Value> {
    if !msg.is_object() {
        return None;
    }
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let mut out = msg.clone();

    match role {
        "system" => {
            let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));
            out["content"] = json!(truncate_text(&text, MAX_SYSTEM_GUIDANCE_CHARS));
            Some(out)
        }
        "user" => {
            let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));
            out["content"] = json!(truncate_text(&text, MAX_USER_CHARS));
            Some(out)
        }
        "assistant" => {
            let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));
            out["content"] = json!(summarize_free_text(&text, MAX_ASSISTANT_CHARS));
            let mut tool_calls: Vec<Value> = Vec::new();
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    if let Some(p) = project_tool_call(tc) {
                        tool_calls.push(p);
                    }
                }
            }
            if !tool_calls.is_empty() {
                out["tool_calls"] = Value::Array(tool_calls);
            } else {
                out.as_object_mut().unwrap().remove("tool_calls");
            }
            Some(out)
        }
        "tool" => {
            let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));
            out["content"] = json!(summarize_tool_output(&text));
            Some(out)
        }
        _ => {
            if conservative {
                let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));
                out["content"] = json!(truncate_text(&text, MAX_ASSISTANT_CHARS));
                return Some(out);
            }
            None
        }
    }
}

fn project_messages_conservative(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|m| project_conversation_message(m, true))
        .collect()
}

fn project_tools(tools: &[Value]) -> (Vec<Value>, (usize, usize, usize, usize)) {
    let original_chars = tools_size(&Value::Array(tools.to_vec()));
    let mut projected: Vec<Value> = Vec::new();
    for tool in tools {
        if !tool.is_object() {
            continue;
        }
        if tool.get("type").and_then(|t| t.as_str()) != Some("function") {
            continue;
        }
        let function = tool.get("function").unwrap_or(tool);
        let name = function.get("name").and_then(|n| n.as_str());
        let Some(name) = name else { continue };
        let mut projected_function = Map::new();
        projected_function.insert("name".to_string(), json!(name));
        if let Some(params) = function.get("parameters") {
            projected_function
                .insert("parameters".to_string(), project_schema(params, 0));
        }
        if let Some(strict) = function.get("strict") {
            projected_function.insert("strict".to_string(), strict.clone());
        }
        projected.push(json!({"type": "function", "function": projected_function}));
    }
    let n = projected.len();
    let projected_chars = tools_size(&Value::Array(projected.clone()));
    (
        projected,
        (tools.len(), n, original_chars, projected_chars),
    )
}

fn project_schema(schema: &Value, depth: usize) -> Value {
    if depth >= 6 {
        return json!({"type": "object"});
    }
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                if !SCHEMA_KEEP_KEYS.contains(&key.as_str()) {
                    continue;
                }
                match key.as_str() {
                    "properties" if value.is_object() => {
                        let props: Map<String, Value> = value
                            .as_object()
                            .unwrap()
                            .iter()
                            .map(|(p, ps)| (p.clone(), project_schema(ps, depth + 1)))
                            .collect();
                        out.insert("properties".to_string(), Value::Object(props));
                    }
                    "items" => {
                        out.insert("items".to_string(), project_schema(value, depth + 1));
                    }
                    "oneOf" | "anyOf" | "allOf" if value.is_array() => {
                        out.insert(
                            key.clone(),
                            Value::Array(
                                value
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .take(6)
                                    .map(|i| project_schema(i, depth + 1))
                                    .collect(),
                            ),
                        );
                    }
                    "additionalProperties" if value.is_object() => {
                        out.insert(
                            "additionalProperties".to_string(),
                            project_schema(value, depth + 1),
                        );
                    }
                    _ => {
                        out.insert(key.clone(), value.clone());
                    }
                }
            }
            if out.is_empty() {
                json!({"type": "object"})
            } else {
                Value::Object(out)
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(6)
                .map(|i| project_schema(i, depth + 1))
                .collect(),
        ),
        _ => schema.clone(),
    }
}

fn looks_like_agentic_cli(messages: &[Value], tools: &[Value]) -> bool {
    let tool_names: BTreeSet<String> = tools
        .iter()
        .map(tool_name)
        .filter(|n| !n.is_empty())
        .collect();
    if tool_names
        .iter()
        .any(|n| AGENTIC_TOOL_NAMES.contains(&n.as_str()))
    {
        return true;
    }
    for msg in messages {
        if !msg.is_object() {
            continue;
        }
        let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));
        if looks_like_harness_user(&text) || looks_like_harness_system(&text) {
            return true;
        }
    }
    false
}

fn choose_tail_start(messages: &[Value]) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut start = messages.len() - 1;
    let mut total_chars = 0usize;
    let mut kept = 0usize;

    for idx in (0..messages.len()).rev() {
        let cost = message_cost(&messages[idx]);
        if kept > 0 && (kept >= MAX_TAIL_MESSAGES || total_chars + cost > MAX_TAIL_CHARS) {
            break;
        }
        start = idx;
        total_chars += cost;
        kept += 1;
    }
    start
}

fn expand_tail_for_tool_context(messages: &[Value], start: usize) -> usize {
    if start == 0 || messages.is_empty() {
        return start;
    }
    let mut needed_call_ids: BTreeSet<String> = messages[start..]
        .iter()
        .filter(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("tool")
                && m.get("tool_call_id").and_then(|v| v.as_str()).is_some()
        })
        .filter_map(|m| m.get("tool_call_id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    if needed_call_ids.is_empty() {
        return start;
    }

    let mut expanded = start;
    for idx in (0..start).rev() {
        let msg = &messages[idx];
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let call_ids: BTreeSet<String> = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let hits: Vec<String> = call_ids
            .intersection(&needed_call_ids)
            .cloned()
            .collect();
        if !hits.is_empty() {
            expanded = idx;
            for h in hits {
                needed_call_ids.remove(&h);
            }
            if needed_call_ids.is_empty() {
                break;
            }
        }
    }
    expanded
}

fn latest_user_index(messages: &[Value]) -> Option<usize> {
    for idx in (0..messages.len()).rev() {
        if messages[idx].get("role").and_then(|r| r.as_str()) == Some("user") {
            return Some(idx);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

pub fn project_responses_chat_body(body: &Value, preserve: bool) -> (Value, Value) {
    let mut projected = body.clone();
    let messages: Vec<Value> = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let tool_stats = |projected_tools: &[Value]| {
        json!({
            "original_tools": tools.len(),
            "projected_tools": projected_tools.len(),
            "original_tool_chars": tools_size(&Value::Array(tools.clone())),
            "projected_tool_chars": tools_size(&Value::Array(projected_tools.to_vec())),
        })
    };

    if preserve {
        let stats = json!({
            "mode": "preserve", "aggressive": false,
            "original_messages": messages.len(), "projected_messages": messages.len(),
            "original_message_chars": messages_size(&messages),
            "projected_message_chars": messages_size(&messages),
            "original_tools": tools.len(), "projected_tools": tools.len(),
            "original_tool_chars": tools_size(&Value::Array(tools.clone())),
            "projected_tool_chars": tools_size(&Value::Array(tools.clone())),
        });
        return (projected, stats);
    }

    let (projected_tools, _stats) = project_tools(&tools);
    let tool_stats = tool_stats(&projected_tools);
    if !projected_tools.is_empty() {
        projected["tools"] = Value::Array(projected_tools);
    } else if projected.get("tools").is_some() {
        projected["tools"] = Value::Array(vec![]);
    }

    if !looks_like_agentic_cli(&messages, &tools) {
        let conservative = project_messages_conservative(&messages);
        let n = conservative.len();
        let stats = json!({
            "mode": "conservative",
            "aggressive": false,
            "original_messages": messages.len(),
            "projected_messages": n,
            "original_message_chars": messages_size(&messages),
            "projected_message_chars": messages_size(&conservative),
        });
        let mut merged = stats.as_object().unwrap().clone();
        for (k, v) in tool_stats.as_object().unwrap() {
            merged.insert(k.clone(), v.clone());
        }
        projected["messages"] = Value::Array(conservative);
        return (projected, Value::Object(merged));
    }

    let names = tool_call_name_map(&messages);
    let mut preserved_guidance: Vec<String> = Vec::new();
    let mut conversation: Vec<Value> = Vec::new();
    let mut dropped_harness_messages = 0usize;

    for msg in &messages {
        if !msg.is_object() {
            continue;
        }
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = content_to_text(msg.get("content").unwrap_or(&Value::Null));

        if role == "system" {
            if looks_like_harness_system(&text) {
                dropped_harness_messages += 1;
                continue;
            }
            let guidance = truncate_text(&text, MAX_SYSTEM_GUIDANCE_CHARS);
            if !guidance.is_empty() {
                preserved_guidance.push(guidance);
            }
            continue;
        }

        if role == "user" && looks_like_harness_user(&text) {
            dropped_harness_messages += 1;
            continue;
        }

        if let Some(projected_msg) = project_conversation_message(msg, false) {
            conversation.push(projected_msg);
        }
    }

    if conversation.is_empty() {
        conversation = project_messages_conservative(&messages);
    }

    let mut tail_start = choose_tail_start(&conversation);
    tail_start = expand_tail_for_tool_context(&conversation, tail_start);
    let latest_user_idx = latest_user_index(&conversation);

    let mut anchor_user: Option<Value> = None;
    if let Some(latest_user_idx) = latest_user_idx {
        if latest_user_idx < tail_start {
            anchor_user = Some(conversation[latest_user_idx].clone());
        }
    }

    let mut omitted: Vec<Value> = Vec::new();
    for (idx, msg) in conversation.iter().enumerate() {
        if idx >= tail_start {
            break;
        }
        if Some(idx) == latest_user_idx && anchor_user.is_some() {
            continue;
        }
        omitted.push(msg.clone());
    }

    let mut final_messages: Vec<Value> = vec![json!({"role": "system", "content": BASE_SYSTEM_PROMPT})];
    let guidance_message = merge_guidance_messages(&preserved_guidance);
    if !guidance_message.is_empty() {
        final_messages.push(json!({"role": "system", "content": guidance_message}));
    }

    let history = history_summary(&omitted, &names);
    if !history.is_empty() {
        final_messages.push(json!({"role": "system", "content": history}));
    }

    if let Some(anchor) = anchor_user.clone() {
        final_messages.push(anchor);
    }

    final_messages.extend(conversation[tail_start..].to_vec());
    let tail_len = conversation.len() - tail_start;
    projected["messages"] = Value::Array(final_messages.clone());

    let stats = json!({
        "mode": "aggressive",
        "aggressive": true,
        "dropped_harness_messages": dropped_harness_messages,
        "preserved_guidance_messages": preserved_guidance.len(),
        "summarized_history_messages": omitted.len(),
        "anchor_user_preserved": anchor_user.is_some(),
        "tail_messages": tail_len,
        "original_messages": messages.len(),
        "projected_messages": final_messages.len(),
        "original_message_chars": messages_size(&messages),
        "projected_message_chars": messages_size(&final_messages),
    });
    let mut merged = stats.as_object().unwrap().clone();
    for (k, v) in tool_stats.as_object().unwrap() {
        merged.insert(k.clone(), v.clone());
    }
    (projected, Value::Object(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preserve_mode_returns_unchanged() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "tools": []});
        let (projected, stats) = project_responses_chat_body(&body, true);
        assert_eq!(projected, body);
        assert_eq!(stats["mode"], "preserve");
    }

    #[test]
    fn test_truncate_text_chars() {
        let s = "a".repeat(100);
        let t = truncate_text(&s, 50);
        assert!(t.contains("[truncated"));
        assert_eq!(truncate_text("short", 50), "short");
    }

    #[test]
    fn test_summarize_tool_output_short() {
        let s = "line1\nline2";
        assert_eq!(summarize_tool_output(s), s);
    }

    #[test]
    fn test_summarize_tool_output_long() {
        let mut lines: Vec<String> = (0..60).map(|i| format!("line{i}")).collect();
        lines.push("Process exited with code 0".to_string());
        let text = lines.join("\n");
        let summary = summarize_tool_output(&text);
        assert!(summary.contains("Process exited with code 0"));
        assert!(summary.contains("Key output:"));
        assert!(summary.contains("Recent tail:"));
        assert!(summary.contains("[omitted"));
    }

    #[test]
    fn test_agentic_projection() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "You are Claude Code, an agent."},
                {"role": "user", "content": "<environment_context>\nsandbox\n</environment_context> do the thing"},
                {"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "exec_command", "arguments": "{\"cmd\": [\"ls\", \"-la\"]}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "file1\nfile2"},
                {"role": "user", "content": "now summarize"}
            ],
            "tools": [
                {"type": "function", "function": {"name": "exec_command", "description": "run", "parameters": {"type": "object", "properties": {"cmd": {"type": "array", "items": {"type": "string"}}}, "required": ["cmd"]}}}
            ]
        });
        let (projected, stats) = project_responses_chat_body(&body, false);
        assert_eq!(stats["mode"], "aggressive");
        let msgs = projected["messages"].as_array().unwrap();
        // 第一条为固定 BASE_SYSTEM_PROMPT
        assert_eq!(msgs[0]["content"], BASE_SYSTEM_PROMPT);
        // harness system 消息被丢弃
        let joined: String = msgs
            .iter()
            .map(|m| m["content"].as_str().unwrap_or(""))
            .collect();
        assert!(!joined.contains("You are Claude Code"));
        // 工具 schema 中 description 被移除
        let t = &projected["tools"][0]["function"];
        assert!(t.get("description").is_none());
        // tool schema 保留参数结构
        assert!(t["parameters"]["properties"].is_object());
    }
}
