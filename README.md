# codebuddy2api-rs

[原项目 codebuddy2api](https://github.com/ShouZhuo0413/codebuddy2api)（Python / FastAPI）的 **Rust 重写版**。

把已登录的腾讯 **CodeBuddy / WorkBuddy** 桌面端订阅，暴露为本机的 **OpenAI / Anthropic 兼容 API**，供 Codex CLI、Claude Code、CC Switch、ZCode 等客户端直接接入。

> 仅供个人学习研究，与腾讯、WorkBuddy、CodeBuddy、OpenAI、Anthropic 无官方关联。

## 原理

- 读取本机已登录的 CodeBuddy/WorkBuddy 桌面端凭据（auth 文件里的 token / uid / enterpriseId）
- 直接转发到 CodeBuddy 后端 `https://copilot.tencent.com/v2/chat/completions`（该后端本身就是标准 OpenAI chat/completions 协议，含原生 tools / tool_calls / SSE 流式）
- 本地只做三件事：①注入鉴权头 ②OpenAI Chat / OpenAI Responses / Anthropic Messages 三种协议与后端互转 ③可选的脱敏与投影压缩
- token 过期时自动调 `/v2/plugin/auth/token/refresh` 刷新并原子回写 auth 文件（含 WorkBuddy 5.6.0+ `$wbEncrypted` 加密信封的解密/重新加密，对应原项目 issue #23）

## 接口

| 端点 | 说明 |
|------|------|
| `POST /v1/chat/completions` | OpenAI Chat Completions 兼容（原生 tools/tool_calls，流式/非流式） |
| `POST /v1/responses` | OpenAI Responses API 兼容（Codex CLI） |
| `POST /v1/messages` | Anthropic Messages API 兼容（Claude Code / CC Switch） |
| `POST /v1/messages/count_tokens` | Anthropic token 计数 |
| `GET /v1/models` | 模型列表（优先读本机 WorkBuddy product.json，否则内置兜底列表） |
| `GET /health` | 健康检查与凭据状态 |

非流式请求由转换器在本地聚合：后端始终以流式调用，聚合后返回完整对象。

## 构建

```bash
cargo build --release
# 产物: target/release/codebuddy2api.exe
```

## 运行

```bash
# 默认 127.0.0.1:8787
codebuddy2api.exe

# 常用参数
codebuddy2api.exe --port 9000 --api-key mysecret --log converter.log --desensitize
```

启动时会做预检（定位 auth 文件、显示账号与 token 状态）；`--skip-check` 可跳过。

### CLI 参数

| 参数 | 说明 |
|------|------|
| `--host` | 监听地址，默认 `127.0.0.1` |
| `--port` | 监听端口，默认 `8787` |
| `--api-key` | 可选：要求客户端携带的 API key（`Bearer` 或 `X-Api-Key` 头） |
| `--log PATH` | 开启文件日志（请求/响应/投影统计/原始 SSE） |
| `--desensitize` | 零宽脱敏 + system/harness 压缩（缓解后端内容审核误拦） |
| `--no-compact` | 配合 `--desensitize`：跳过压缩，仅做零宽脱敏 |
| `--skip-check` | 跳过启动预检 |

### 环境变量

| 变量 | 说明 |
|------|------|
| `CODEBUDDY_AUTH_DIR` | 指定登录态目录（Docker 场景） |
| `CODEBUDDY2OPENAI_KEY` | 本地 API Key（等价 `--api-key`） |
| `CODEBUDDY2OPENAI_LOG` | 日志路径（等价 `--log`） |
| `WORKBUDDY_AT_REST_SECRET` | 直接提供 WorkBuddy at-rest 密钥（44 字符 base64），免调 Electron |
| `WORKBUDDY_ELECTRON_PATH` | 指定 WorkBuddy 可执行文件路径 |
| `CODEBUDDY_LOSSY_PROJECTION` | 设为 `1` 启用 /v1/responses 的有损投影压缩（默认关闭） |
| `CODEBUDDY_RESPONSES_DESENSITIZE` | 设为 `1` 时 /v1/responses 也走脱敏 |

## 登录态文件位置

- Windows: `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\*.info`
- macOS: `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/*.info`
- Linux: `~/.local/share/CodeBuddyExtension/Data/Public/auth/*.info`

优先使用 `workbuddy-desktop.info`，否则取 mtime 最新的 `.info` 文件。请先在桌面端完成登录。

## 客户端配置示例

OpenAI 兼容客户端（含 Codex CLI 的 `base_url`）指向 `http://127.0.0.1:8787/v1`；Claude Code / CC Switch 的 Anthropic `base_url` 指向 `http://127.0.0.1:8787`。设置了 `--api-key` 时 API key 随意填写该值即可。

## 模型

内置兜底列表：`hy3`、`hy4-preview`、`kimi-k3`、`kimi-k2.8-preview`、`glm-5.3`、`glm-5.3-flash`、`glm-5.2`、`glm-5.1`、`glm-5v-turbo`、`kimi-k2.7`、`kimi-k2.6`、`kimi-k2.5`、`deepseek-v4-pro`、`deepseek-v4.1-flash`、`deepseek-v4-flash`、`minimax-m3-pay`、`hy3-preview-agent`、`auto`。本机装有 WorkBuddy 时优先使用其动态模型目录（自动过滤 text-to-image/video 与内部补全模型）。

## 测试

```bash
cargo test
```

包含从原项目 pytest 测试移植的关键用例：Anthropic/Responses 适配器的请求转换与流式转换（tool_calls 分片合并、usage 合并、cached_tokens 兜底链、sequence_number 连续性、上游错误不能变空成功、incomplete 输出）、身份声明过滤、零宽脱敏、投影压缩等。

## 与原项目的差异

- 完整实现核心转换器（standalone 模式）的全部功能
- 未移植可选的中文 Web 管理后台（账号池/签到/指标），单账号模式
- 日志格式与原版一致，便于对照排障

## ⚠️ 杀毒软件误报

本程序的行为特征（读取登录凭据文件、注入 Authorization 头、零宽字符脱敏、AES-GCM 加解密）容易被杀软启发式引擎误报（如火绒 `Trojan/Intercept.a`）。源码完全开放，可自行审计后编译，并将本项目目录加入杀软信任区。构建时已嵌入版本信息资源以降低误报概率。

## License

MIT（同原项目）
