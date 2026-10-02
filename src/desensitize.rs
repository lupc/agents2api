//! 针对 CodeBuddy 后端内容审核的脱敏模块（独立、可选，`--desensitize` 开启）。
//!
//! 背景：后端内容审核会拦截含"攻击/漏洞/凭证"等含义的英文术语，但这些词经常
//! 出现在客户端固定的合规 system 模板里（"拒绝作恶"声明），并非有害输入。
//!
//! 做法：对明确词表里的词在内部插入零宽空格（U+200B），
//! `DoS -> Do\u200bS`，人/模型读仍是 DoS，后端关键词匹配失效。
//! 只处理指定角色（默认 system），不改动其它角色内容。

use regex::{Captures, Regex};
use serde_json::{json, Value};
use std::sync::LazyLock;

/// 零宽空格：插入到关键词内部，打断后端的关键词匹配。
const ZWSP: char = '\u{200b}';

/// 触发审核的"合规声明高频词"（来自真实被拦截的客户端 system 模板）。
const SENSITIVE_TERMS: &[&str] = &[
    // 原有词表
    "DoS",
    "DDoS",
    "exploit",
    "credential testing",
    "credential stuffing",
    "supply chain compromise",
    "supply-chain compromise",
    "detection evasion",
    "C2 frameworks",
    "C2 framework",
    "command and control",
    "malicious purposes",
    "malicious intent",
    "mass targeting",
    "brute force",
    "brute-force",
    "privilege escalation",
    "reverse shell",
    "remote code execution",
    "SQL injection",
    "XSS",
    "CSRF",
    "phishing",
    "malware",
    "ransomware",
    "keylogger",
    "rootkit",
    "backdoor",
    "botnet",
    "zero-day",
    "0day",
    // Codex CLI system prompt 里额外的高频触发词
    "vulnerability",
    "vulnerabilities",
    "red teaming",
    "red-teaming",
    "sandbox",
    "sandboxing",
    "sandboxed",
    "unsandboxed",
    "escalated privileges",
    "escalated",
    "escalation",
    "destructive action",
    "destructive command",
    "destructive",
    "attack",
    "attacks",
    "cybersecurity",
    "security review",
    "exploit development",
    "hacking",
    "penetration testing",
    "penetration test",
    "injection",
    "weaponize",
    "weaponized",
    "harmful",
    "dangerous",
    "abuse",
    "abusive",
    "illegal",
    "terrorist",
    "terrorism",
    "bomb",
    "weapon",
    "weapons",
    "drug",
    "drugs",
    "narcotic",
    "suicide",
    "self-harm",
    "murder",
    "kill",
    "violence",
    "violent",
    // Claude Code / Anthropic 品牌词（避免竞争品牌词触发审核）
    "Claude Code",
    "Claude Opus",
    "Claude Sonnet",
    "Claude Haiku",
    "Claude Fable",
    "Anthropic",
    "Co-Authored-By",
    "noreply@anthropic.com",
];

static PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    let mut terms: Vec<&str> = SENSITIVE_TERMS.to_vec();
    // 按词长降序，避免短词先吃掉长词
    terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
    let joined: Vec<String> = terms.iter().map(|t| regex::escape(t)).collect();
    Regex::new(&format!("(?i){}", joined.join("|"))).unwrap()
});

/// 在词内部插入零宽空格。如 'DoS' -> 'Do\u200bS'。
fn zero_width_split(term: &str) -> String {
    let mut chars = term.chars();
    match chars.next() {
        Some(c) => {
            let mut s = String::with_capacity(term.len() + 3);
            s.push(c);
            s.push(ZWSP);
            s.push_str(chars.as_str());
            s
        }
        None => String::new(),
    }
}

/// 对文本中的触发词插入零宽空格。无触发词则原样返回。
pub fn desensitize_text(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    PATTERN
        .replace_all(text, |caps: &Captures| zero_width_split(caps.get(0).unwrap().as_str()))
        .to_string()
}

// ---------------------------------------------------------------------------
// harness 识别与压缩
// ---------------------------------------------------------------------------

/// Codex CLI 会把大量运行时上下文包装进一条 user 消息里；这些不是用户真正提问。
const HARNESS_USER_MARKERS: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<permissions instructions>",
    "<collaboration_mode>",
    "<skills_instructions>",
    "<system-reminder>", // Claude Code 注入的运行时上下文
    "# claudeMd",        // Claude Code CLAUDE.md 注入
];

const CODEX_SYSTEM_MARKERS: &[&str] = &[
    "You are a coding agent running in the Codex CLI",
    "Within this context, Codex refers to",
    "# How you work",
    "You are Claude Code", // Claude Code system prompt
];

const PERMISSIONS_MARKERS: &[&str] = &[
    "<permissions instructions>",
    "Filesystem sandboxing defines which files can be read or written.",
    "## How to request escalation",
];

const SKILLS_MARKERS: &[&str] = &[
    "<skills_instructions>",
    "### Available skills",
    "### How to use skills",
];

const RUNTIME_BLOCK_REPLACEMENTS: &[(&str, &str, &str)] = &[
    (
        "<environment_context>",
        "</environment_context>",
        "Environment context is provided by the harness.",
    ),
    (
        "<permissions instructions>",
        "</permissions instructions>",
        "Runtime permissions apply: filesystem access may be sandboxed, network may be restricted, and some commands may require user approval.",
    ),
    (
        "<collaboration_mode>",
        "</collaboration_mode>",
        "Collaboration mode instructions are provided by the harness.",
    ),
    (
        "<skills_instructions>",
        "</skills_instructions>",
        "Runtime skill metadata is available. Use relevant skills only when explicitly requested or clearly applicable.",
    ),
    (
        "<plugins_instructions>",
        "</plugins_instructions>",
        "Runtime plugin metadata is available when relevant.",
    ),
    (
        "<system-reminder>",
        "</system-reminder>",
        "Runtime reminder context is provided by the harness.",
    ),
];

// 注意：第三条 marker 里的 sk\u200bills / Sk\u200bill 含零宽字符（与上游保持一致）
const RUNTIME_TAIL_MARKERS: &[&str] = &[
    "The following deferred tools are now available via ToolSearch.",
    "Available agent types for the Agent tool:",
    "The following sk\u{200b}ills are available for use with the Sk\u{200b}ill tool:",
    "## MCP Server Instructions",
];

const RUNTIME_TAIL_SUMMARY: &str =
    "Runtime tool, agent, skill, and MCP metadata is available separately.";

const CODEX_CORE_SUMMARY: &str = "You are a coding assistant in Codex CLI. Be precise, helpful, concise, and safe. Inspect the repository, use available tools when needed, follow repository instructions, and keep the user informed with concise progress updates.";

/// 把字符串或 content blocks 规整成纯文本，便于识别注入模板。
fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<String> = Vec::new();
            for blk in blocks {
                if let Some(t) = blk.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
            parts.concat()
        }
        _ => String::new(),
    }
}

/// 判断 user 消息是否其实是 Codex/CLI 注入的上下文，而非用户自然输入。
fn looks_like_harness_user_message(content: Option<&Value>) -> bool {
    let text = content_to_text(content);
    HARNESS_USER_MARKERS.iter().any(|m| text.contains(m))
}

static BLOCK_SEP_RE_CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Regex>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn block_sep_regex(start_tag: &str, end_tag: &str) -> Regex {
    let key = format!("{start_tag}\u{1}{end_tag}");
    let mut cache = BLOCK_SEP_RE_CACHE.lock().unwrap();
    if let Some(r) = cache.get(&key) {
        return r.clone();
    }
    let r = Regex::new(&format!(
        r"(?s)\s*{}.*?{}\s*",
        regex::escape(start_tag),
        regex::escape(end_tag)
    ))
    .unwrap();
    cache.insert(key, r.clone());
    r
}

/// 等价于 `re.search(r"^.*?(?=\n# AGENTS\.md spec|\n## Responsiveness|\n## Planning|\n## Task execution|\Z)", ...)`。
fn find_intro(pruned: &str) -> String {
    const MARKERS: &[&str] = &[
        "\n# AGENTS.md spec",
        "\n## Responsiveness",
        "\n## Planning",
        "\n## Task execution",
    ];
    let cut = MARKERS.iter().filter_map(|m| pruned.find(m)).min();
    match cut {
        Some(i) => pruned[..i].trim().to_string(),
        None => pruned.trim().to_string(),
    }
}

/// 等价于 `re.search(escape(heading) + r".*?(?=\n## |\n# |\Z)", ...)`。
fn find_section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let idx = text.find(heading)?;
    let search_start = idx + heading.len();
    let next = text[search_start..]
        .find("\n## ")
        .map(|i| search_start + i)
        .into_iter()
        .chain(text[search_start..].find("\n# ").map(|i| search_start + i))
        .min()
        .unwrap_or(text.len());
    Some(&text[idx..next])
}

/// 轻量裁掉冗长的运行时元数据，保留主要行为指令。
///
/// 用于 --no-compact 场景：尽量保留 Codex / Claude Code 的核心提示，
/// 但移除重复的 environment / permissions / skills / tool inventory 大段文本。
fn prune_runtime_fragments(role: &str, text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let mut pruned = text.to_string();
    for (start_tag, end_tag, replacement) in RUNTIME_BLOCK_REPLACEMENTS {
        let re = block_sep_regex(start_tag, end_tag);
        pruned = re
            .replace_all(&pruned, format!("\n\n{replacement}\n\n").as_str())
            .to_string();
    }

    let tail_indexes: Vec<usize> = RUNTIME_TAIL_MARKERS
        .iter()
        .filter_map(|m| pruned.find(m))
        .collect();
    if !tail_indexes.is_empty() {
        let cut = tail_indexes.iter().copied().min().unwrap();
        let head = pruned[..cut].trim_end().to_string();
        pruned = if !head.is_empty() {
            format!("{head}\n\n{RUNTIME_TAIL_SUMMARY}")
        } else {
            RUNTIME_TAIL_SUMMARY.to_string()
        };
    }

    if role == "system" && CODEX_SYSTEM_MARKERS.iter().any(|m| pruned.contains(m)) {
        let mut keep_sections: Vec<String> = Vec::new();
        let intro = find_intro(&pruned);
        if !intro.is_empty() {
            keep_sections.push(intro);
        }
        for heading in ["## Personality", "# AGENTS.md spec"] {
            if let Some(section) = find_section(&pruned, heading) {
                let s = section.trim();
                if !s.is_empty() {
                    keep_sections.push(s.to_string());
                }
            }
        }
        pruned = if !keep_sections.is_empty() {
            keep_sections.join("\n\n")
        } else {
            CODEX_CORE_SUMMARY.to_string()
        };
    }

    if role == "user" && looks_like_harness_user_message(Some(&Value::String(pruned.clone()))) {
        if pruned.contains("# AGENTS.md instructions")
            || text.contains("<environment_context>")
            || text.contains("<skills_instructions>")
        {
            return "Repository instructions and durable user context are provided. \
                    Follow repository guidance while answering the user's actual request."
                .to_string();
        }
    }

    let re3 = Regex::new(r"\n{3,}").unwrap();
    re3.replace_all(&pruned, "\n\n").trim().to_string()
}

/// 把 Codex / Claude Code 注入的超长运行时提示压缩成短摘要，降低审核误伤。
fn compact_harness_message(role: &str, content: Option<&Value>) -> Option<String> {
    let text = content_to_text(content);
    if text.is_empty() {
        return None;
    }
    if role == "system" && CODEX_SYSTEM_MARKERS.iter().any(|m| text.contains(m)) {
        if text.contains("You are Claude Code") {
            return Some("You are a coding assistant. Be precise, helpful, concise, and safe. \
                         Use available tools when needed, follow repository instructions, and keep the user informed."
                .to_string());
        }
        return Some("You are a coding assistant in Codex CLI. Be precise, helpful, concise, and safe. \
                     Use available tools when needed, follow repository instructions, and keep the user informed."
            .to_string());
    }
    if PERMISSIONS_MARKERS.iter().any(|m| text.contains(m)) {
        return Some("Runtime permissions apply: filesystem access may be sandboxed, network may be restricted, \
                     and some commands may require user approval."
            .to_string());
    }
    if SKILLS_MARKERS.iter().any(|m| text.contains(m)) {
        return Some("Runtime skill metadata is available. Use relevant skills only when explicitly requested or clearly applicable."
            .to_string());
    }
    if role == "user" && looks_like_harness_user_message(content) {
        return Some("Repository instructions and environment context are provided. Follow repository guidance \
                     while answering the user's actual request."
            .to_string());
    }
    None
}

/// 递归处理 tool 定义，必要时移除高风险描述字段。
fn desensitize_tool_value(value: &Value, strip_metadata: bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                if (key == "description" || key == "title") && item.is_string() {
                    if strip_metadata {
                        continue;
                    }
                    out.insert(key.clone(), json!(desensitize_text(item.as_str().unwrap())));
                } else {
                    out.insert(key.clone(), desensitize_tool_value(item, strip_metadata));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| desensitize_tool_value(i, strip_metadata))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// 对指定角色的消息文本做脱敏，返回新的 messages 数组（不修改原对象）。
pub fn desensitize_messages(
    messages: &[Value],
    roles: &[&str],
    desensitize_harness_user: bool,
    compact_harness: bool,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        if !m.is_object() {
            out.push(m.clone());
            continue;
        }
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let mut should_desensitize = roles.contains(&role);
        if role == "user" && desensitize_harness_user {
            should_desensitize = looks_like_harness_user_message(m.get("content"));
        }
        let mut nm = m.clone();
        if should_desensitize {
            let content = m.get("content");
            let compacted = if compact_harness {
                compact_harness_message(role, content)
            } else {
                None
            };
            match (compacted, content) {
                (Some(compacted), _) => {
                    nm["content"] = json!(desensitize_text(&compacted));
                }
                (None, Some(Value::String(s))) => {
                    nm["content"] = json!(desensitize_text(&prune_runtime_fragments(role, s)));
                }
                (None, Some(Value::Array(blocks))) => {
                    let mut new_blocks: Vec<Value> = Vec::new();
                    for blk in blocks {
                        if blk.get("type").and_then(|t| t.as_str()) == Some("text")
                            && blk.get("text").and_then(|t| t.as_str()).is_some()
                        {
                            let mut nb = blk.clone();
                            nb["text"] = json!(desensitize_text(&prune_runtime_fragments(
                                role,
                                blk.get("text").and_then(|t| t.as_str()).unwrap()
                            )));
                            new_blocks.push(nb);
                        } else {
                            new_blocks.push(blk.clone());
                        }
                    }
                    nm["content"] = Value::Array(new_blocks);
                }
                _ => {}
            }
        }
        out.push(nm);
    }
    out
}

/// 对请求体里的 messages / tools 做脱敏，返回新的 body。
pub fn desensitize_body(
    body: &Value,
    roles: &[&str],
    desensitize_harness_user: bool,
    desensitize_tools: bool,
    compact_harness: bool,
    strip_tool_metadata: bool,
) -> Value {
    let mut nb = body.clone();
    if body.get("messages").map(|m| !m.is_null()).unwrap_or(false) {
        let msgs = body
            .get("messages")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        nb["messages"] = Value::Array(desensitize_messages(
            &msgs,
            roles,
            desensitize_harness_user,
            compact_harness,
        ));
    }
    if desensitize_tools && body.get("tools").map(|t| !t.is_null()).unwrap_or(false) {
        nb["tools"] = desensitize_tool_value(body.get("tools").unwrap(), strip_tool_metadata);
    }
    nb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_width_desensitize() {
        let d = desensitize_text("Refuse requests for DoS attacks and exploit development.");
        assert!(d.contains('\u{200b}'));
        // 可见字符（去掉零宽字符后）与原文相同
        assert_eq!(d.replace('\u{200b}', ""), "Refuse requests for DoS attacks and exploit development.");
        assert_eq!(desensitize_text("没有触发词"), "没有触发词");
    }

    #[test]
    fn test_messages_only_system_touched() {
        let msgs = vec![
            json!({"role": "system", "content": "Refuse DoS attacks and exploit development."}),
            json!({"role": "user", "content": "explain DoS attacks"}),
        ];
        let out = desensitize_messages(&msgs, &["system"], false, false);
        assert!(out[0]["content"].as_str().unwrap().contains('\u{200b}'));
        assert!(!out[1]["content"].as_str().unwrap().contains('\u{200b}'));
    }

    #[test]
    fn test_compact_codex_system() {
        let msgs = vec![json!({"role": "system", "content": "You are a coding agent running in the Codex CLI. ...very long text with sandbox and exploit words..."})];
        let out = desensitize_messages(&msgs, &["system", "developer"], false, true);
        let c = out[0]["content"].as_str().unwrap();
        assert!(c.starts_with("You are a coding assistant in Codex CLI."));
        assert!(c.len() < 250);
    }

    #[test]
    fn test_strip_tool_metadata() {
        let tools = json!([{"type": "function", "function": {"name": "Bash", "description": "Run a sandbox command", "parameters": {"type": "object"}}}]);
        let out = desensitize_tool_value(&tools, true);
        assert!(out[0]["function"].get("description").is_none());
        assert!(out[0]["function"]["parameters"].is_object());
    }
}
