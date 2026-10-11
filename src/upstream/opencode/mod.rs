//! OpenCode 凭据上游：把本机 OpenCode 的 `auth.json` 里已有的 API key
//! （如 `nvidia` / `modelscope`）暴露成 OpenAI 兼容 API。
//!
//! 与 CodeBuddy 一样，这些都是标准 OpenAI chat/completions 兼容的后端：
//! - `prepare_chat` 直通 body，只注入 `Authorization: Bearer <key>`；
//! - `translate_stream` 恒等转发。

use serde_json::Value;

use super::{passthrough_stream, ByteStream, Upstream, UpstreamRequest};

pub mod auth;
pub mod zen_free;

/// 一个本地 OpenCode 里配置的、标准 OpenAI 兼容的云服务商。
pub struct OpenCodeKeyUpstream {
    /// 上游标识（= auth.json 里的 provider 名，如 `nvidia`）。
    name: String,
    /// chat/completions 的完整 URL。
    chat_url: String,
    /// models 列表 URL。
    models_url: String,
    /// 该服务商的 API key。
    api_key: String,
}

impl OpenCodeKeyUpstream {
    pub fn new(name: String, base_url: String, api_key: String) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            name,
            chat_url: format!("{base}/chat/completions"),
            models_url: format!("{base}/models"),
            api_key,
        }
    }
}

#[async_trait::async_trait]
impl Upstream for OpenCodeKeyUpstream {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String> {
        if self.api_key.is_empty() {
            return Err(format!("{} 缺少 API key（auth.json）", self.name));
        }
        Ok(UpstreamRequest {
            url: self.chat_url.clone(),
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            )],
            body: chat.clone(),
        })
    }

    fn translate_stream(&self, raw: ByteStream) -> ByteStream {
        passthrough_stream(raw)
    }

    fn aggregate(&self, raw: &[u8]) -> Value {
        super::aggregate_openai_sse(raw)
    }

    /// 直接向上游拉取 `/v1/models` 目录；失败返回空列表。
    async fn models(&self) -> Vec<String> {
        let client = match reqwest::Client::builder().build() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let resp = client
            .get(&self.models_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await;
        let Ok(resp) = resp else {
            return Vec::new();
        };
        let Ok(json) = resp.json::<Value>().await else {
            return Vec::new();
        };
        json.get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn health_info(&self) -> Value {
        serde_json::json!({
            "credential": {
                "source": "opencode auth.json",
                "provider": self.name,
                "has_key": !self.api_key.is_empty(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_builds_chat_and_models_url() {
        let u = OpenCodeKeyUpstream::new(
            "nvidia".to_string(),
            "https://integrate.api.nvidia.com/v1/".to_string(),
            "nvapi-x".to_string(),
        );
        assert_eq!(u.name(), "nvidia");
        assert_eq!(u.chat_url, "https://integrate.api.nvidia.com/v1/chat/completions");
        assert_eq!(u.models_url, "https://integrate.api.nvidia.com/v1/models");
    }

    #[tokio::test]
    async fn test_prepare_chat_injects_bearer() {
        let u = OpenCodeKeyUpstream::new(
            "modelscope".to_string(),
            "https://api-inference.modelscope.cn/v1".to_string(),
            "ms-abc".to_string(),
        );
        let req = u
            .prepare_chat(&json!({"model": "m", "messages": []}))
            .await
            .unwrap();
        assert_eq!(req.headers[0].0, "Authorization");
        assert_eq!(req.headers[0].1, "Bearer ms-abc");
        assert_eq!(req.body["model"], "m");
    }

    #[tokio::test]
    async fn test_prepare_chat_errors_without_key() {
        let u = OpenCodeKeyUpstream::new(
            "nvidia".to_string(),
            "https://integrate.api.nvidia.com/v1".to_string(),
            String::new(),
        );
        assert!(u.prepare_chat(&json!({"messages": []})).await.is_err());
    }
}
