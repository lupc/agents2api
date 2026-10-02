# codebuddy2api-rs

[原项目 codebuddy2api](https://github.com/ShouZhuo0413/codebuddy2api)（Python / FastAPI）的 **Rust 重写版**，并在其基础上做了**上游可插拔重构**，新增 **Trae** 上游支持。

把已登录的腾讯 **CodeBuddy / WorkBuddy**、字节 **Trae** 桌面端订阅，暴露为本机的 **OpenAI / Anthropic 兼容 API**，供 Codex CLI、Claude Code、CC Switch、ZCode 等客户端直接接入。

> 仅供个人学习研究，与腾讯、字节、WorkBuddy、CodeBuddy、Trae、OpenAI、Anthropic 无官方关联。

## 架构：可插拔多上游

上游通过 [`Upstream`](src/upstream/mod.rs) trait 抽象，与协议转换层完全解耦：

```rust
#[async_trait]
pub trait Upstream: Send + Sync {
    fn name(&self) -> &str;
    /// 把标准 OpenAI Chat body 包装成该上游的 wire body，并给出 URL + 鉴权头
    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String>;
    /// 上游原始 SSE → 标准 OpenAI Chat SSE（CodeBuddy 恒等，Trae 需事件转换）
    fn translate_stream(&self, raw: ByteStream) -> ByteStream;
    /// 非流式：聚合为 chat.completion
    fn aggregate(&self, raw: &[u8]) -> Value;
    async fn models(&self) -> Vec<String>;
    async fn health_info(&self) -> Value;
}
```

新增一个后端只需实现这个 trait 并在 `main.rs` 注册——`routes.rs`、三个协议适配器（Anthropic / Responses / Chat）、脱敏与投影层都无需改动。

**多上游按路径前缀隔离**：

| 路径 | 上游 |
|------|------|
| `/v1/*`、`/health` | 默认上游（`--upstream`，默认 `codebuddy`） |
| `/codebuddy/v1/*`、`/codebuddy/health` | 强制 CodeBuddy |
| `/trae/v1/*`、`/trae/health` | 强制 Trae |

未配置的上游前缀返回 404 并列出可用上游，互不影响。

## 原理

- **CodeBuddy**：读本机 auth `.info` 凭据 → 直连 `copilot.tencent.com/v2/chat/completions`（该后端本身就是标准 OpenAI 协议，含原生 tools / SSE）。token 过期自动刷新并原子回写，支持 WorkBuddy 5.6.0+ `$wbEncrypted` 加密信封（issue #23）。
- **Trae**：解密 `storage.json` 里 AES-128-CBC 加密的登录态 blob 取 JWT → 调 `trae-api-cn.mchost.guru/api/agent/v3/llm_utils_chat`（**字节私有协议，非 OpenAI 兼容**）→ 把私有 SSE 事件实时转成 OpenAI chunk。请求体需包装成 `{messages, function, config_name, model, ...}` 且 content 必须是数组。

## 接口

| 端点 | 说明 |
|------|------|
| `POST /v1/chat/completions` | OpenAI Chat Completions 兼容（原生 tools/tool_calls，流式/非流式） |
| `POST /v1/responses` | OpenAI Responses API 兼容（Codex CLI） |
| `POST /v1/messages` | Anthropic Messages API 兼容（Claude Code / CC Switch） |
| `POST /v1/messages/count_tokens` | Anthropic token 计数 |
| `GET /v1/models` | 模型列表（按上游返回各自目录） |
| `GET /health` | 健康检查、凭据状态、上游名 |

以上端点在 `/{upstream}` 前缀下同样可用。

## 构建

```bash
cargo build --release
# 产物: target/release/codebuddy2api.exe
```

## 运行

```bash
# 默认 127.0.0.1:8787，默认上游 codebuddy
codebuddy2api.exe

# 默认走 Trae；两个上游都自动探测并注册
codebuddy2api.exe --upstream trae --api-key mysecret --log converter.log

# 只启用某一个上游
codebuddy2api.exe --disable-upstream codebuddy
```

### CLI 参数

| 参数 | 说明 |
|------|------|
| `--host` / `--port` | 监听地址与端口（默认 `127.0.0.1:8787`） |
| `--api-key` | 可选：要求客户端携带的 API key |
| `--log PATH` | 开启文件日志（请求 / 响应 / 投影统计 / 原始 SSE） |
| `--upstream NAME` | 无前缀时使用的默认上游（`codebuddy` / `trae`） |
| `--disable-upstream NAME` | 禁用某上游（可重复） |
| `--desensitize` / `--no-compact` | 零宽脱敏（+ 是否压缩 harness） |
| `--skip-check` | 跳过启动预检 |
| `--codebuddy-backend URL` | 覆盖 CodeBuddy 后端地址 |
| `--trae-chat-host URL` | 覆盖 Trae chat host |
| `--trae-function NAME` | 覆盖 Trae `function` 字段（默认 `solo_work_lite`） |
| `--trae-storage-json PATH` | 显式指定 Trae storage.json |

### 环境变量

| 变量 | 说明 |
|------|------|
| `CODEBUDDY_AUTH_DIR` | CodeBuddy 登录态目录 |
| `CODEBUDDY2OPENAI_KEY` / `CODEBUDDY2OPENAI_LOG` | 等价 `--api-key` / `--log` |
| `WORKBUDDY_AT_REST_SECRET` / `WORKBUDDY_ELECTRON_PATH` | WorkBuddy at-rest 密钥 |
| `TRAE_STORAGE_JSON` | Trae storage.json 路径 |
| `TRAE_CHAT_HOST` / `TRAE_FUNCTION` | Trae 端点与 function |
| `TRAE_APP_VERSION_CODE` / `TRAE_IDE_VERSION` | Trae 版本号（见下方说明） |
| `TRAE_SALT_A..D` | 覆盖 Trae 解密 salt（逗号分隔十进制字节） |
| `CODEBUDDY_LOSSY_PROJECTION` | `1` = 启用 /v1/responses 有损投影 |
| `CODEBUDDY_RESPONSES_DESENSITIZE` | `1` = /v1/responses 也脱敏 |

## 登录态位置

**CodeBuddy / WorkBuddy**（`.info` 文件，优先 `workbuddy-desktop.info`）：
- Windows `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\`
- macOS `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/`
- Linux `~/.local/share/CodeBuddyExtension/Data/Public/auth/`

**Trae**（`storage.json`，自动探测 4 个变体）：
- `%APPDATA%\Trae CN\User\globalStorage\storage.json`
- `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`
- `%APPDATA%\Trae\User\globalStorage\storage.json`
- `%APPDATA%\TRAE SOLO\User\globalStorage\storage.json`

登录态是 `iCubeAuthInfo://icube.cloudide` 键下的加密 blob（`[6B header][32B random][AES-128-CBC]`），密钥由 `SHA512(SHA512(random) ‖ salt)` 派生。

## Trae 支持的现状与已知限制

Trae 上游是**逆向实现的私有协议**，实测状态如下：

- ✅ 登录态解密、JWT 提取、鉴权头注入、请求体包装、路径路由均正常（已对本机真实 Trae CN 登录态验证，成功通过鉴权）
- ✅ 私有 SSE → OpenAI chunk 的事件转换（`event:` 行格式、`output`/`done`/`error`/`token_usage` 等）
- ✅ 错误码映射（`1001`→401、`4008/1005`→402、`4001`→400、`4011`→429）
- ⚠️ **`app_version_code` 需与客户端版本匹配**：上游对未知值返回 `4001 param is invalid`。默认值未必适用于你的版本，若遇到 4001，请用抓包得到真实值后设 `TRAE_APP_VERSION_CODE=<int64>`。
- ⚠️ 上游协议无官方文档、可能随客户端更新漂移；`create_agent_task`/Work 通道未实现（当前只做 IDE Agent 单请求端点）。
- ⚠️ Trae 的 token 刷新与客户端登录可能互斥，故**只读不刷新**；过期时返回明确提示，请重开 Trae 登录。

遇到 4001 时的排查建议：用 `--log` 启动，日志里会打印「发往上游的 WIRE BODY」与「TRAE RAW SSE」，据此比对真实客户端请求。

## 测试

```bash
cargo test   # 75 个用例
```

覆盖：三个协议适配器（请求转换 + 流式事件转换 + 回归用例）、Trae 解密往返与篡改检测、Trae 私有事件映射（含实测 `event:` 行格式）、错误码映射、多上游路由分发、身份过滤、零宽脱敏、投影压缩。

## ⚠️ 杀毒软件误报

本程序的行为特征（读取登录凭据、注入 Authorization 头、解密登录态、零宽字符脱敏）容易被杀软启发式引擎误报（如火绒 `Trojan/Intercept.a`）。源码完全开放，可自行审计后编译，并将项目目录加入信任区。

## License

MIT（同原项目）

