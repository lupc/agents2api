# agents2api

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
| `/buddy/v1/*` | Buddy（原 codebuddy + workbuddy 合并；旧前缀 `/codebuddy`、`/workbuddy` 仍兼容可用） |
| `/trae/v1/*`、`/trae/health` | 强制 Trae |
| `/nvidia/v1/*`、`/nvidia/health` | OpenCode `auth.json` 里的 NVIDIA 服务商 |
| `/modelscope/v1/*`、`/modelscope/health` | OpenCode `auth.json` 里的 ModelScope 服务商 |

> **关于 Buddy**：CodeBuddy 与 WorkBuddy 是同一个上游（共用登录态文件 `CodeBuddyExtension/.../auth/*.info` 与后端 `copilot.tencent.com`），已合并为单一 `buddy` 上游；旧路径前缀 `/codebuddy/*`、`/workbuddy/*` 自动映射到 buddy，历史配置不失效。

> **关于 `nvidia` / `modelscope`**：这两个上游读取本机 **OpenCode** 的 `auth.json`（`~/.local/share/opencode/auth.json`），把其中标准 OpenAI 兼容的服务商 key 暴露出来。每个 provider 以自身名字注册为一个独立上游，走后端各自的 OpenAI 兼容端点。

未配置的上游前缀返回 404 并列出可用上游，互不影响。

## 原理

- **CodeBuddy**：读本机 auth `.info` 凭据 → 直连 `copilot.tencent.com/v2/chat/completions`（该后端本身就是标准 OpenAI 协议，含原生 tools / SSE）。token 过期自动刷新并原子回写，支持 WorkBuddy 5.6.0+ `$wbEncrypted` 加密信封（issue #23）。
- **Trae**：解密 `storage.json` 里 AES-128-CBC 加密的登录态 blob 取 JWT → 调 `trae-api-cn.mchost.guru/api/agent/v3/llm_utils_chat`（**字节私有协议，非 OpenAI 兼容**）→ 把私有 SSE 事件实时转成 OpenAI chunk。请求体需包装成 `{messages, function, config_name, model, ...}` 且 content 必须是数组。
- **OpenCode（nvidia / modelscope）**：读本机 OpenCode 的 `auth.json`，取其中的 API key → 直连各服务商的 **标准 OpenAI 兼容**端点（NVIDIA `integrate.api.nvidia.com/v1`、ModelScope `api-inference.modelscope.cn/v1`）→ 注入 `Authorization: Bearer <key>` 后恒等转发。模型列表直接向上游 `/v1/models` 拉取。

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

## 管理后台（Web UI）

访问 `http://127.0.0.1:8787/admin/`（Vue3 + ElementPlus 单页应用，构建后内嵌进二进制，无需额外服务）。

| 页面 | 功能 |
|------|------|
| 汇总统计 | 请求数/成功率/Tokens/P95 耗时卡片、请求趋势图、模型速率榜（P50/P95/TTFT）、按天用量表 |
| 上游配置 | 各上游启用开关、codebuddy 后端地址与 trae chat host 覆盖、默认上游切换，保存即热重载 |
| 模型同步 | 从上游目录接口拉取真实模型目录（Buddy: console/personal/models 15 个；Trae: batch_get_detail_param 20 个），含展示名/厂商/积分倍率/上下文/图片支持，支持每日定时同步 |
| 一键签到 | Trae 每日签到，逐次结果与按日趋势（CodeBuddy/WorkBuddy 暂不支持，接口已预留） |
| 积分看板 | Trae 剩余/总额/已消耗积分、积分包明细与过期时间 |
| 日志 | 查看滚动日志最新文件的尾部（`--log-dir` 启用时） |

对应 WebAPI（`/admin/api/*`）：`overview`、`stats/usage`、`stats/models`、`upstreams`、`upstreams/reload`、`config`(GET/PUT)、`models`、`sync`、`checkin/trae`、`checkin/results`、`credits/trae`、`logs`、`health`。

- 统计数据落在 `<data_dir>/stats.json`（按天聚合，保留 90 天，内存权威 + 2s 周期落盘）
- 上游配置落在 `<data_dir>/upstreams.json`（文件值覆盖 CLI/env）
- 数据目录默认 `%APPDATA%agents2api`（Windows），可用 `--data-dir` 改；`--no-admin` 关闭后台

### 前端开发

```bash
cd web
npm install
npm run dev        # 开发模式：/admin 代理到 127.0.0.1:8787，改前端刷新即可
npm run build      # 产出 web/dist，cargo build 时由 rust-embed 内嵌
```

仓库已包含构建好的 `web/dist`，纯后端改动无需重新构建前端。

## 构建

### 环境要求

- **Rust 1.75+**（本项目使用 2021 edition；在 1.99 上验证通过）
- Windows 需要 MSVC 工具链（`stable-x86_64-pc-windows-msvc`）
- 无需额外系统依赖（HTTP/TLS 由 rustls 提供，不依赖 OpenSSL）

安装/更新工具链：

```bash
rustup update stable
```

### 标准构建

```bash
git clone <本仓库>
cd agents2api

# 开发构建（快，未优化，产物大）
cargo build
# → target/debug/agents2api(.exe)

# 发布构建（推荐，opt-level=2 + strip）
cargo build --release
# → target/release/agents2api(.exe)  约 7.3 MB
```

产物是**单文件静态二进制**，无需运行时依赖，直接拷走即可运行：

```bash
# Windows
target\release\agents2api.exe --help

# Linux / macOS
./target/release/agents2api --help
```

### 运行测试

```bash
cargo test                       # 全部 111 个用例
cargo test trae                  # 只跑 Trae 相关
cargo test -- --nocapture        # 显示输出
```

### 检查与格式化

```bash
cargo check                      # 只做类型检查，比 build 快
cargo clippy --all-targets       # 静态检查（当前零警告）
cargo fmt                        # 格式化
```

提交前建议依次跑：`cargo fmt` → `cargo clippy --all-targets` → `cargo test`。

### 打包分发

构建后把二进制与文档一起复制到 `dist/`：

```bash
# Linux / macOS
mkdir -p dist
cp target/release/agents2api dist/
cp README.md dist/

# Windows (Git Bash)
mkdir -p dist
cp target/release/agents2api.exe dist/
cp README.md dist/
```

`dist/` 已在 `.gitignore` 中，不会误提交。

### 跨平台编译

```bash
# 先添加目标平台
rustup target add x86_64-unknown-linux-gnu
rustup target add aarch64-apple-darwin

# 编译到指定目标
cargo build --release --target x86_64-unknown-linux-gnu
```

> 跨平台交叉编译到 Linux/macOS 需要对应的链接器（如 `cross` 或 `cargo-zigbuild`）。
> Windows 上编译 Windows 版无需任何额外配置。

### 构建产物说明

| 路径 | 说明 |
|------|------|
| `target/debug/agents2api` | 开发构建，含调试符号，编译快 |
| `target/release/agents2api` | 发布构建，已 strip，体积小、运行快 |
| `target/release/build/` | 构建脚本产物（winresource 写入的版本信息资源） |

### 常见问题

**杀毒软件报毒**：构建出的 exe 可能被杀软启发式误报（如火绒报 `Trojan/Intercept.a`）。
构建后若 exe 立即消失，请把项目目录加入杀软信任区（详见文末「杀毒软件误报」）。

**`拒绝访问 (os error 5)`**：目标 exe 正在运行，占用了文件。先终止进程再重新构建：

```bash
# Windows
taskkill /F /IM agents2api.exe

# Linux / macOS
pkill -f agents2api
```

**构建很慢**：首次构建需编译全部依赖（约 1–2 分钟），后续增量构建只需几秒。
可用 `cargo build --release -j 8` 指定并行度。

## 运行

```bash
# 默认 127.0.0.1:8787，默认上游 codebuddy
agents2api.exe

# 默认走 Trae；两个上游都自动探测并注册
agents2api.exe --upstream trae --api-key mysecret --log converter.log

# 只启用某一个上游
agents2api.exe --disable-upstream codebuddy
```

### CLI 参数

| 参数 | 说明 |
|------|------|
| `--host` / `--port` | 监听地址与端口（默认 `127.0.0.1:8787`） |
| `--api-key` | 可选：要求客户端携带的 API key |
| `--log PATH` | 开启文件日志（请求 / 响应 / 投影统计 / 原始 SSE），单文件追加、不滚动 |
| `--log-dir DIR` | 按日期分目录的滚动日志：`DIR/<日期>/converter-NNN.log`，单文件上限 100MB 超限自动滚动；与 `--log` 同时给出时 `--log` 优先 |
| `--upstream NAME` | 无前缀时使用的默认上游（`codebuddy` / `trae`） |
| `--disable-upstream NAME` | 禁用某上游（可重复） |
| `--desensitize` / `--no-compact` | 零宽脱敏（+ 是否压缩 harness） |
| `--skip-check` | 跳过启动预检 |
| `--data-dir DIR` | 数据目录（统计/签到/后台配置），默认 %APPDATA%agents2api |
| `--no-admin` | 关闭管理后台 |
| `--codebuddy-backend URL` | 覆盖 CodeBuddy 后端地址 |
| `--trae-chat-host URL` | 覆盖 Trae chat host |
| `--trae-storage-json PATH` | 显式指定 Trae storage.json |
| `--opencode-auth-json PATH` | 显式指定 OpenCode auth.json |

### 环境变量

| 变量 | 说明 |
|------|------|
| `CODEBUDDY_AUTH_DIR` | CodeBuddy 登录态目录 |
| `CODEBUDDY2OPENAI_KEY` / `CODEBUDDY2OPENAI_LOG` | 等价 `--api-key` / `--log` |
| `CODEBUDDY2OPENAI_LOG_DIR` | 等价 `--log-dir` |
| `AGENTS2API_DATA_DIR` | 等价 `--data-dir` |
| `WORKBUDDY_AT_REST_SECRET` / `WORKBUDDY_ELECTRON_PATH` | WorkBuddy at-rest 密钥 |
| `TRAE_STORAGE_JSON` | Trae storage.json 路径 |
| `TRAE_CHAT_HOST` | Trae 上游 host |
| `TRAE_APP_VERSION_CODE` / `TRAE_IDE_VERSION` | Trae 版本号（见下方说明） |
| `TRAE_SALT_A..D` | 覆盖 Trae 解密 salt（逗号分隔十进制字节） |
| `OPENCODE_AUTH_JSON` | OpenCode auth.json 路径（不设则自动探测） |
| `CODEBUDDY_LOSSY_PROJECTION` | `1` = 启用 /v1/responses 有损投影 |
| `CODEBUDDY_RESPONSES_DESENSITIZE` | `1` = /v1/responses 也脱敏 |

### 日志

两种文件日志方式，均同时输出到控制台（stderr）：

```bash
# 方式一：单文件追加（不滚动，适合短期调试）
agents2api.exe --log converter.log

# 方式二：按日期分目录 + 滚动（推荐长期运行）
agents2api.exe --log-dir logs
```

`--log-dir` 的产出结构：

```
logs/
└── 2026-10-11/
    ├── converter-001.log   ← 单文件上限 100MB，写满滚动为 -002
    └── converter-002.log
```

- 重启进程会续写当天最新的文件（不覆盖历史）
- 跨天自动切换到新日期目录
- 每条日志带时间戳与请求 id（`[rid]`），含请求/响应体、上游 wire body、原始 SSE，
  便于排查上游协议问题

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

**OpenCode**（`auth.json`，取其中的 `nvidia` / `modelscope` 项）：
- Windows `%APPDATA%\opencode\auth.json`（也探测 `%USERPROFILE%\.local\share\opencode\`）
- macOS / Linux `~/.local/share/opencode/auth.json`（也探测 `~/.config/opencode/`）

## Trae 支持的现状

Trae 上游是**逆向实现的私有协议**（参考 [smart-open/TraeWorkAssistant](https://github.com/smart-open/TraeWorkAssistant) 的真机验证实现），当前状态：

- ✅ **端到端可用**：登录态解密、鉴权、对话、流式、工具调用、thinking 全部实测通过
- ✅ 已实测的模型：`deepseek-v4-flash`、`deepseek-v4-pro`、`glm-5.3`、`glm-5.2`、`kimi-k3` 等
- ✅ 支持 `reasoning_content`（思维链）、`tool_calls`（含增量分片合并）、usage

### 协议要点（踩过的坑）

这几处与原项目早期实现不同，都是真机验证后才修对的：

| 项 | 正确做法 |
|----|---------|
| 鉴权头 | **`x-ide-token: <裸 JWT>`**，不发 `Authorization`。带 `Cloud-IDE-JWT ` 前缀会被网关在 token 校验前拒绝（401 / code 1001） |
| 版本号 | 在 **header**（`x-app-version-code` / `x-ide-version-code` = `20260811`），不在 body |
| 请求体必填 | `prompt_max_tokens: 168000`（缺失或传 1M 都返回 4001）、`model_name`（`<config>__dev`）、`user_id`、`device_id`、`machine_id`、`workspace_id`、`mode: FunctionCall` 等 |
| 模型映射 | `config_name`（展示名）+ `model_name`（内部名）双字段；如 `deepseek-v4-flash` → `DeepSeek-V4-Flash` / `deepseek_v4_flash__dev` |
| function 视图 | 多数模型用 `solo_work_lite`；`deepseek-v4.1-flash`、`doubao-seed-code`、`glm-5.3-flash`、`qwen3.8-flash`、`kimi-k2.8-preview` 只在 `solo_agent` 下可用 |
| SSE 事件名 | **只从 `event:` 行取**，payload 的 `type` 字段不参与判定 |
| SSE 文本字段 | 取 **`response`**（不是 `content`）；思维链在 `reasoning_content` |
| 工具调用 | `function_call` 子对象；**一次调用会拆成多个增量块**（首块给 id+name，后续块只给 arguments），需按 index 累积 |
| 错误码 | `4001`（param invalid / model config mismatch）是**请求级**错误，与账号无关，直接透传 400 |

若上游返回 `4001`，排查顺序：①`prompt_max_tokens` 是否为 168000；②模型名是否拼写正确；③该模型是否需要在 `solo_agent` 视图。

可用 `--log` 启动，日志会打印「发往上游的 WIRE BODY」与「TRAE RAW SSE」便于比对。

### 环境变量

| 变量 | 说明 |
|------|------|
| `TRAE_CHAT_HOST` | 上游 host（默认 `https://trae-api-cn.mchost.guru`） |
| `TRAE_IDE_VERSION` / `TRAE_IDE_VERSION_CODE` | 客户端版本 / 版本代码（默认 `0.1.50` / `20260811`） |
| `TRAE_APP_ID` | 应用 ID |
| `TRAE_STORAGE_JSON` | 显式指定 storage.json 路径 |
| `TRAE_SALT_A..D` | 覆盖解密 salt（逗号分隔十进制字节） |

## 测试

```bash
cargo test            # 全部 111 个用例
cargo test trae       # 只跑 Trae 相关
```

覆盖范围：

- **协议适配器**：Anthropic / Responses 的请求转换与流式事件转换（tool_calls 分片合并、
  usage 合并、cached_tokens 兜底链、sequence_number 连续性、上游错误不能变空成功、
  incomplete 输出）
- **Trae 上游**：storage.json 解密往返与篡改检测、私有事件映射（含实测 `event:` 行格式、
  拆分式 tool_calls 全链路聚合回归）、请求头与必填字段、模型映射与 function 视图选择、
  错误码映射
- **多上游**：路径前缀路由分发、按上游隔离的模型列表、未知上游 404
- **其余**：身份声明过滤、零宽脱敏、Responses 投影压缩、CodeBuddy 加解密往返

## ⚠️ 杀毒软件误报

本程序的行为特征（读取登录凭据、注入 Authorization 头、解密登录态、零宽字符脱敏）容易被杀软启发式引擎误报（如火绒 `Trojan/Intercept.a`）。源码完全开放，可自行审计后编译，并将项目目录加入信任区。

## License

MIT（同原项目）

