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

    /// 健康检查附带的账号/状态信息。
    async fn health_info(&self) -> Value;
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
