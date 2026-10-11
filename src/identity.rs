//! 仅从 system / developer 文本中移除客户端身份声明（identity declarations）。

use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;

static DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:you\s+are\b|your\s+(?:designated\s+)?identity\b|you\s+have\s+been\s+invoked\b|you\s+are\s+powered\s+by\b|你(?:是|的身份是|的角色是|由)|您(?:是|的身份是|的角色是|由))").unwrap()
});

static IDENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:zcode|codex|claude(?:\s+code)?|codebuddy|workbuddy|opencode|sisyphus|anthropic|openai|chatgpt|gemini|deepseek|qwen|kimi|glm|gpt|AI|LLM|assistant|agent|model)\b|人工智能|智能助手|编程助手|编码助手|语言模型|智能体").unwrap()
});

static OBLIGATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^you\s+are\s+(?:required|expected|allowed|not allowed|forbidden|responsible|operating|working|running|using|to)\b").unwrap()
});

static HEADINGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^#{1,6}\s+(?:ZCode|Codex|Claude Code|CodeBuddy|WorkBuddy|OpenCode)\s+").unwrap()
});

static BRANCH_TEMPLATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Main branch \(you will usually use this for PRs\):").unwrap()
});

/// 等价于 Python `re.split(r"(?<=[.!?。！？])(?=\s|$)", s)`：
/// 在句末标点后（且其后是空白或结尾）切开。
fn split_sentences(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let char_indices: Vec<(usize, char)> = s.char_indices().collect();
    for (i, &(byte_idx, ch)) in char_indices.iter().enumerate() {
        if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
            let next_is_ws_or_end = char_indices
                .get(i + 1)
                .is_none_or(|&(_, c)| c.is_whitespace());
            if next_is_ws_or_end {
                let end = byte_idx + ch.len_utf8();
                parts.push(&s[start..end]);
                start = end;
            }
        }
    }
    parts.push(&s[start..]);
    parts
}

/// 等价于 Python `text.splitlines(keepends=True)`（覆盖常见换行符）。
fn split_lines_keepends(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                let end = i + 1;
                lines.push(&s[start..end]);
                start = end;
                i = end;
            }
            b'\r' => {
                // \r\n 作为单个换行
                let end = if bytes.get(i + 1) == Some(&b'\n') { i + 2 } else { i + 1 };
                lines.push(&s[start..end]);
                start = end;
                i = end;
            }
            _ => i += 1,
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

fn strip_bullet_prefix(line: &str) -> &str {
    // re.sub(r"^\s*(?:[-*]\s+|#{1,6}\s+)?", "", line)
    let t = line.trim_start();
    let after_ws = line.len() - t.len();
    let rest = &line[after_ws..];
    if rest.starts_with('-') || rest.starts_with('*') {
        let r = rest[1..].trim_start();
        // 只有后面跟着空白才算 bullet（[-*]\s+）
        if rest.len() > 1 && rest[1..].starts_with(|c: char| c.is_whitespace()) {
            return &line[after_ws + (rest.len() - r.len())..];
        }
        return line;
    }
    if rest.starts_with('#') {
        let hashes = rest.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) {
            let after_hashes = &rest[hashes..];
            if after_hashes.starts_with(' ') {
                let r = after_hashes.trim_start();
                return &line[line.len() - r.len()..];
            }
        }
    }
    line
}

/// 对一段 system 文本逐行移除身份声明句。
pub fn filter_system_text(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in split_lines_keepends(text) {
        let candidate = strip_bullet_prefix(line);
        // 限制只移除陈述句，保留后续指令
        let pieces = split_sentences(candidate);
        let mut kept: Vec<&str> = Vec::new();
        let mut removed = false;
        for piece in pieces {
            let clean = piece.trim();
            if DECLARATION.is_match(clean)
                && IDENTITY.is_match(clean)
                && !OBLIGATION.is_match(clean)
            {
                removed = true;
            } else {
                kept.push(piece);
            }
        }
        let mut line_out = if removed {
            let joined: String = kept.concat();
            joined.trim_start().to_string()
        } else {
            line.to_string()
        };
        if removed && line_out.trim().is_empty() {
            continue;
        }
        // 保留分支值与章节语义，去掉固定客户端措辞
        line_out = BRANCH_TEMPLATE
            .replace(&line_out, "Main branch:")
            .to_string();
        line_out = HEADINGS.replace(&line_out, "# ").to_string();
        lines.push(line_out);
    }
    lines.concat()
}

/// 返回新的 body；绝不修改 user/assistant/tool 消息或 tool schema。
pub fn filter_system_identity(body: &Value) -> Value {
    let mut out = body.clone();
    let Some(messages) = out.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return out;
    };
    let mut new_messages: Vec<Value> = Vec::new();
    for message in messages.iter() {
        let role_ok = message.get("role").and_then(|r| r.as_str())
            == Some("system")
            || message.get("role").and_then(|r| r.as_str()) == Some("developer");
        if !role_ok {
            new_messages.push(message.clone());
            continue;
        }
        match message.get("content") {
            Some(Value::String(content)) => {
                let filtered = filter_system_text(content);
                if !filtered.trim().is_empty() {
                    let mut m = message.clone();
                    m["content"] = json!(filtered);
                    new_messages.push(m);
                }
            }
            Some(Value::Array(blocks)) => {
                let mut new_blocks: Vec<Value> = Vec::new();
                for block in blocks {
                    let is_text_block = block.get("type").and_then(|t| t.as_str())
                        == Some("text")
                        || block.get("type").and_then(|t| t.as_str()) == Some("input_text")
                        || block.get("type").and_then(|t| t.as_str()) == Some("output_text");
                    if is_text_block
                        && block.get("text").and_then(|t| t.as_str()).is_some()
                    {
                        let filtered = filter_system_text(
                            block.get("text").and_then(|t| t.as_str()).unwrap(),
                        );
                        if !filtered.trim().is_empty() {
                            let mut b = block.clone();
                            b["text"] = json!(filtered);
                            new_blocks.push(b);
                        }
                    } else {
                        new_blocks.push(block.clone());
                    }
                }
                if !new_blocks.is_empty() {
                    let mut m = message.clone();
                    m["content"] = Value::Array(new_blocks);
                    new_messages.push(m);
                }
            }
            _ => new_messages.push(message.clone()),
        }
    }
    out["messages"] = Value::Array(new_messages);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_identity_declaration_sentence() {
        let text = "You are ZCode, an interactive coding agent. Do the work well.";
        let filtered = filter_system_text(text);
        assert!(!filtered.contains("You are ZCode"));
        assert!(filtered.contains("Do the work well."));
    }

    #[test]
    fn keeps_obligation_sentences() {
        let text = "You are required to follow the user's instructions.";
        assert_eq!(filter_system_text(text), text);
    }

    #[test]
    fn preserves_other_lines() {
        let text = "First line stays.\nYou are powered by GLM. Second line stays.";
        let filtered = filter_system_text(text);
        assert!(filtered.contains("First line stays."));
        assert!(filtered.contains("Second line stays."));
        assert!(!filtered.contains("You are powered by GLM."));
    }

    #[test]
    fn branch_template_replaced() {
        let text = "Main branch (you will usually use this for PRs): main";
        let filtered = filter_system_text(text);
        assert!(filtered.contains("Main branch: main"));
        assert!(!filtered.contains("you will usually use this for PRs"));
    }

    #[test]
    fn chinese_declaration_removed() {
        let text = "你是 ZCode。 请帮我修 bug。";
        let filtered = filter_system_text(text);
        assert!(!filtered.contains("你是"));
        assert!(filtered.contains("请帮我修 bug。"));
    }
}
