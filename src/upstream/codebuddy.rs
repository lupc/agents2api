//! CodeBuddy / WorkBuddy 上游。
//!
//! 后端 `https://copilot.tencent.com/v2/chat/completions` 本身就是标准 OpenAI
//! chat/completions 协议（含原生 tools / tool_calls / SSE 流式），因此：
//! - `prepare_chat` 直通 body，只需注入鉴权头；
//! - `translate_stream` 是恒等转发。
//!
//! 例外：`tool_choice` 需规范化。腾讯后端该字段是 Go `string` 类型，收到 OpenAI
//! 的对象形态会报 code 11101（实测）。后端接受 `"auto"` / `"required"` /
//! 工具名字符串。

use serde_json::{json, Value};

use crate::auth::CredentialManager;
use crate::models::get_available_models;

use super::{passthrough_stream, ByteStream, Upstream, UpstreamRequest};

pub struct CodeBuddyUpstream {
    backend: String,
    cred: CredentialManager,
}

impl CodeBuddyUpstream {
    pub fn new(cred: CredentialManager, backend: Option<String>) -> Self {
        Self {
            backend: backend.unwrap_or_else(|| crate::auth::BACKEND.to_string()),
            cred,
        }
    }
}

/// 把 OpenAI/Anthropic 的对象形态 `tool_choice` 规范化为腾讯后端接受的字符串。
///
/// - `"none"` / `"auto"` / `"required"` → 原样
/// - `{"type":"any"}` / `{"type":"required"}` → `"required"`
/// - `{"type":"function","function":{"name":X}}` / `{"type":"tool","name":X}` → `"X"`
/// - 其它对象（无名字）→ `"auto"`
fn normalize_tool_choice(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(choice) = obj.get("tool_choice").cloned() else {
        return;
    };
    let normalized = match &choice {
        Value::String(_) => None, // 已是字符串，不动
        Value::Object(m) => {
            let t = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let name = m
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .or_else(|| m.get("name").and_then(|v| v.as_str()))
                .unwrap_or("");
            let s = if t == "none" {
                "none"
            } else if t == "any" || t == "required" {
                "required"
            } else if !name.is_empty() {
                name
            } else {
                "auto"
            };
            Some(json!(s))
        }
        _ => Some(json!("auto")),
    };
    if let Some(v) = normalized {
        obj.insert("tool_choice".to_string(), v);
    }
}

#[async_trait::async_trait]
impl Upstream for CodeBuddyUpstream {
    fn name(&self) -> &str {
        "codebuddy"
    }

    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String> {
        let mut body = chat.clone();
        normalize_tool_choice(&mut body);
        let headers = self.cred.get_headers().await?;
        Ok(UpstreamRequest {
            url: format!("{}/v2/chat/completions", self.backend),
            headers,
            body,
        })
    }

    fn translate_stream(&self, raw: ByteStream) -> ByteStream {
        passthrough_stream(raw)
    }

    fn aggregate(&self, raw: &[u8]) -> Value {
        super::aggregate_openai_sse(raw)
    }

    async fn models(&self) -> Vec<String> {
        get_available_models()
    }

    async fn health_info(&self) -> Value {
        match self.cred.summary().await {
            Ok(s) => serde_json::json!({"credential": s}),
            Err(e) => serde_json::json!({"credential_error": e}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(body: Value) -> Value {
        let mut b = body;
        normalize_tool_choice(&mut b);
        b
    }

    #[test]
    fn test_object_named_choice_to_tool_name() {
        let out = norm(json!({"tool_choice": {"type": "function", "function": {"name": "Bash"}}}));
        assert_eq!(out["tool_choice"], "Bash");
    }

    #[test]
    fn test_flat_named_choice_to_tool_name() {
        // Responses API 的扁平形态
        let out = norm(json!({"tool_choice": {"type": "function", "name": "Read"}}));
        assert_eq!(out["tool_choice"], "Read");
    }

    #[test]
    fn test_anthropic_tool_choice_forms() {
        let out = norm(json!({"tool_choice": {"type": "tool", "name": "Bash"}}));
        assert_eq!(out["tool_choice"], "Bash");
        let out = norm(json!({"tool_choice": {"type": "any"}}));
        assert_eq!(out["tool_choice"], "required");
        let out = norm(json!({"type": "auto", "tool_choice": {"type": "auto", "function": {"name": ""}}}));
        assert_eq!(out["tool_choice"], "auto");
    }

    #[test]
    fn test_string_choices_untouched() {
        for s in ["auto", "none", "required"] {
            let out = norm(json!({"tool_choice": s}));
            assert_eq!(out["tool_choice"], s);
        }
    }

    #[test]
    fn test_missing_or_nonobject_body_safe() {
        let out = norm(json!({"messages": []}));
        assert!(out.get("tool_choice").is_none());
        normalize_tool_choice(&mut json!("not-an-object")); // 不 panic
    }

    #[test]
    fn test_no_tool_choice_key_untouched() {
        let out = norm(json!({"model": "x"}));
        assert!(out.get("tool_choice").is_none());
    }
}
