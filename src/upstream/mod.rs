//! 上游抽象层：把「本地 OpenAI/Anthropic 协议」与「具体后端」解耦。
//!
//! 每个上游实现 [`Upstream`]：
//! - [`Upstream::prepare_chat`] 负责把标准 OpenAI Chat body 转成该上游的 wire body，
//!   并给出 URL 与鉴权头（CodeBuddy 是直通 + Bearer；Trae 需要包装成私有协议）。
//! - [`Upstream::translate_stream`] 把上游的原始 SSE 字节流转成标准 OpenAI Chat SSE
//!   （CodeBuddy 后端本身就是 OpenAI 协议，为恒等转发；Trae 是私有事件，需要状态机转换）。
//! - 非流式统一为「上游始终流式 + 收集原始字节 + [`Upstream::aggregate`]」，
//!   避免每个 handler 各写一套聚合逻辑。

pub mod codebuddy;
pub mod codebuddy_catalog;
pub mod opencode;
pub mod sse;
pub mod trae;

use bytes::Bytes;
use futures::stream::BoxStream;
use serde_json::Value;

pub use sse::{err_event, looks_like_content_filter_text, ChatAggregator, StreamStats};

/// 上游 SSE 字节流类型别名。
pub type ByteStream = BoxStream<'static, reqwest::Result<Bytes>>;

/// 一次上游调用的准备结果。
pub struct UpstreamRequest {
    /// 完整的请求 URL。
    pub url: String,
    /// 上游鉴权/上下文头（不透明，由各上游自行构造）。
    pub headers: Vec<(String, String)>,
    /// 发往该上游的 wire body（可能已从标准 OpenAI Chat 包装为私有协议）。
    pub body: Value,
}

/// 上游后端。
#[async_trait::async_trait]
pub trait Upstream: Send + Sync {
    /// 上游标识，如 `codebuddy` / `trae`。
    fn name(&self) -> &str;

    /// 把标准 OpenAI Chat body 转成该上游的 wire body，并算出 URL 与鉴权头。
    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String>;

    /// 把上游原始 SSE 字节流转成标准 OpenAI Chat SSE 字节流。
    fn translate_stream(&self, raw: ByteStream) -> ByteStream;

    /// 非流式：把完整的上游 SSE 响应体聚合为一个 `chat.completion` 对象。
    fn aggregate(&self, raw: &[u8]) -> Value;

    /// 该上游可用的模型 id 列表。
    async fn models(&self) -> Vec<String>;

    /// 远程模型目录同步：拉取带元数据的完整目录（倍率/档位/上下文/图片）。
    /// 默认不支持（返回 Err）；支持的上游覆盖之。
    async fn sync_catalog(&self) -> Result<Vec<ModelInfo>, String> {
        Err(format!("上游 {} 不支持远程目录同步", self.name()))
    }

    /// 健康检查附带的账号/状态信息。
    async fn health_info(&self) -> Value;
}

/// 目录里的单个模型（对齐参考实现的 `WbModel` 语义）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    /// 模型 id（客户端请求的 model 字段，小写）
    pub id: String,
    /// 展示名
    #[serde(default)]
    pub display: String,
    /// 厂商（按模型 id 前缀推断）
    #[serde(default)]
    pub vendor: String,
    /// 积分倍率（0 = 未知）
    #[serde(default)]
    pub rate: f64,
    /// 支持的 reasoning effort 档位（升序）
    #[serde(default)]
    pub efforts: Vec<String>,
    /// 上下文长度（token，0 = 未知）
    #[serde(default)]
    pub context_length: u64,
    /// 单次最大输出（0 = 未知）
    #[serde(default)]
    pub max_tokens: u64,
    /// 是否支持图片输入
    #[serde(default)]
    pub supports_image: bool,
}

// ---------------------------------------------------------------------------
// 上游注册表（内部 RwLock，支持管理后台热重载）
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Default)]
struct RegistryInner {
    map: HashMap<String, Arc<dyn Upstream>>,
    default: String,
}

/// 上游注册表：读多写少，写只发生在启动与管理后台 reload。
pub struct UpstreamRegistry {
    inner: RwLock<RegistryInner>,
}

impl UpstreamRegistry {
    pub fn new(map: HashMap<String, Arc<dyn Upstream>>, default: String) -> Self {
        Self {
            inner: RwLock::new(RegistryInner { map, default }),
        }
    }

    /// 按名字取上游；`None` / 空串取默认上游。
    pub fn get(&self, name: Option<&str>) -> Option<Arc<dyn Upstream>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let key = match name {
            Some(n) if !n.is_empty() => n,
            _ => inner.default.as_str(),
        };
        inner.map.get(key).cloned()
    }

    /// 快照（供管理后台列举）。
    pub fn snapshot(&self) -> Vec<(String, Arc<dyn Upstream>)> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn default_name(&self) -> String {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).default.clone()
    }

    /// 整体替换（reload）：接管另一个 registry 的内容。
    pub fn replace(&self, other: UpstreamRegistry) {
        let other = other.inner.into_inner().unwrap_or_else(|e| e.into_inner());
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.map = other.map;
        inner.default = other.default;
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).map.len()
    }

}

/// 用 [`ChatAggregator`] 聚合标准 OpenAI SSE 的默认实现（CodeBuddy / Trae 转义后均适用）。
pub fn aggregate_openai_sse(raw: &[u8]) -> Value {
    let text = String::from_utf8_lossy(raw);
    let mut agg = ChatAggregator::new();
    for line in text.lines() {
        agg.feed_line(line.trim_end_matches('\r'));
    }
    agg.finish()
}

/// 恒等转发：适用于上游本身就是标准 OpenAI SSE 的情况（CodeBuddy）。
pub fn passthrough_stream(raw: ByteStream) -> ByteStream {
    raw
}
