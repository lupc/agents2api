# agents2api

> 🚀 **Turn your desktop AI subscriptions into a local OpenAI / Claude compatible API — in one step**

**No API key, no top-up, no code changes** — read the credentials of AI desktop apps already
signed in on your machine, and expose them as a standard **OpenAI / Anthropic compatible API
gateway** that **AI coding tools** such as Claude Code, Codex CLI, Cursor, Cline, Roo Code and
CC Switch can plug into directly.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](#build)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg)](#cross-compilation)
[![OpenAI](https://img.shields.io/badge/API-OpenAI%20compatible-10a37f.svg)](#endpoints)
[![Anthropic](https://img.shields.io/badge/API-Anthropic%20compatible-d97757.svg)](#endpoints)
[![Single Binary](https://img.shields.io/badge/deploy-single%20binary-success.svg)](#build)

`AI Gateway` · `LLM Proxy` · `OpenAI API` · `Claude API` · `Anthropic API` ·
`Claude Code` · `Codex CLI` · `Cursor` · `Cline` · `Roo Code` · `LLM API` ·
`free API` · `DeepSeek` · `Kimi` · `GLM` · `Qwen` · `OpenRouter alternative`

> ⚠️ For personal learning and research only. Not affiliated with Tencent, ByteDance,
> WorkBuddy, CodeBuddy, Trae, OpenAI or Anthropic.

**Languages:** **English** · [简体中文](README.md)

---

## ✨ Why agents2api

| Pain point | How agents2api solves it |
|------------|--------------------------|
| Claude Code / Codex CLI are great, but API keys are expensive | Reuse the AI quota you **already subscribe to** on desktop — no extra cost |
| Every AI client speaks a different protocol | **One gateway** speaks OpenAI / Responses / Anthropic at the same time |
| The Python build has heavy deps and is painful to deploy | **Rewritten in Rust**, a **single static binary** — copy and run, no runtime deps |
| Adding a model / upstream means editing code | `Upstream` trait is **pluggable**, multiple upstreams isolated by path prefix |
| No idea how fast each turn ran or what it cost | Built-in **admin UI**: speed, tokens, cache hit rate, check-in, credits |

---

## 🎯 Supported AI clients (use it right inside these tools)

Claude Code · Codex CLI · Cursor · Cline · Roo Code · CC Switch · ZCode ·
Lingma · any client that supports a **custom OpenAI / Anthropic Base URL**

> Point the tool's `Base URL` at this service (e.g. `http://127.0.0.1:8787/v1`) — no API key needed.

---

## 📡 Supported upstreams (AI model sources)

Upstreams are abstracted behind the [`Upstream`](src/upstream/mod.rs) trait, fully decoupled
from the protocol conversion layer:

```rust
#[async_trait]
pub trait Upstream: Send + Sync {
    fn name(&self) -> &str;
    /// Wrap a standard OpenAI Chat body into this upstream's wire body,
    /// returning the URL + auth headers.
    async fn prepare_chat(&self, chat: &Value) -> Result<UpstreamRequest, String>;
    /// Upstream raw SSE → standard OpenAI Chat SSE
    /// (CodeBuddy is identity; Trae needs event conversion).
    fn translate_stream(&self, raw: ByteStream) -> ByteStream;
    /// Non-streaming: aggregate into a chat.completion.
    fn aggregate(&self, raw: &[u8]) -> Value;
    async fn models(&self) -> Vec<String>;
    async fn health_info(&self) -> Value;
}
```

Adding a new backend only requires implementing this trait and registering it in `main.rs` —
`routes.rs`, the three protocol adapters (Anthropic / Responses / Chat), and the
desensitization & projection layers stay untouched.

**Multiple upstreams isolated by path prefix**:

| Path | Upstream | Notes |
|------|----------|-------|
| `/v1/*`, `/health` | Default upstream (`--upstream`) | Used when no prefix is given |
| `/buddy/v1/*` | Buddy | Tencent CodeBuddy + WorkBuddy (merged; legacy prefixes `/codebuddy`, `/workbuddy` still work) |
| `/trae/v1/*` | Trae | ByteDance Trae (reverse-engineered private protocol, supports thinking / tools) |
| `/nvidia/v1/*` | NVIDIA | NVIDIA provider from OpenCode `auth.json` |
| `/modelscope/v1/*` | ModelScope | ModelScope provider from OpenCode `auth.json` |
| `/opencode-free/v1/*` | OpenCode Zen Free | **Anonymous free channel, no login required**, `-free` models only |

> **About Buddy**: CodeBuddy and WorkBuddy are the same upstream (they share the credential file
> `CodeBuddyExtension/.../auth/*.info` and the backend `copilot.tencent.com`), merged into a single
> `buddy` upstream. Legacy path prefixes `/codebuddy/*` and `/workbuddy/*` map to buddy
> automatically, so old configs keep working.

> **About `nvidia` / `modelscope`**: reads the local **OpenCode** `auth.json`
> (`~/.local/share/opencode/auth.json`) and exposes the standard OpenAI-compatible provider keys
> inside it. Each provider is registered as its own upstream.

> **About `opencode-free`**: this is **OpenCode Zen's anonymous free channel** — no local login
> required. Zen's free models (id containing `-free`) move validation from "user account" to
> "client identity"; the gateway requires: ① the fixed sentinel key `Authorization: Bearer public`;
> ② client identity headers `user-agent: opencode/<version>`, `x-opencode-client: cli`,
> `x-opencode-project`, `x-opencode-request`; ③ session affinity headers
> `x-opencode-session` / `-session-id` / `x-session-affinity` in the form
> `ses_<12hex(timestamp)><14base62>`; ④ **request body `stream: true` with `tools` containing
> both `read` and `bash`** (otherwise `403 FreeTierError` / `400`). This upstream injects the
> identity headers and fills in these gate fields automatically, and filters `-free` models out of
> `/models`. Failing any condition is rejected (`403 FreeTierError`, `400 MissingSessionID`
> or `429 FreeUsageLimitError`).

An unconfigured upstream prefix returns 404 with the list of available upstreams, without
affecting anything else.

---

## 🔌 Endpoints (OpenAI / Anthropic in one)

| Endpoint | Description |
|----------|-------------|
| `POST /v1/chat/completions` | **OpenAI Chat Completions compatible** (native tools/tool_calls, streaming/non-streaming) |
| `POST /v1/responses` | **OpenAI Responses API compatible** (Codex CLI) |
| `POST /v1/messages` | **Anthropic Messages API compatible** (Claude Code / CC Switch) |
| `POST /v1/messages/count_tokens` | Anthropic token counting |
| `GET /v1/models` | Model list (per-upstream catalog) |
| `GET /health` | Health check, credential status, upstream name |

All of the above are also available under the `/{upstream}` prefix.

---

## ⚡ 30-second quick start

```bash
# 1) Build (a single static binary, no runtime deps)
cargo build --release
# → target/release/agents2api(.exe)

# 2) Run (auto-detects and registers all available upstreams)
agents2api.exe

# 3) Point your AI client at it
#    OpenAI-compatible client →  http://127.0.0.1:8787/v1
#    Anthropic (Claude Code)   →  http://127.0.0.1:8787
```

```bash
# Set the default upstream / add an access key / write logs
agents2api.exe --upstream trae --api-key mysecret --log converter.log

# Enable only one upstream
agents2api.exe --disable-upstream buddy
```

Open the admin UI: `http://127.0.0.1:8787/admin/`

---

## 🖥 Admin UI (Web)

A Vue3 + Element Plus single-page app, embedded into the binary after build — no extra service.

| Page | Features |
|------|----------|
| Overview | Requests / success rate / tokens / P95 latency cards, request trend chart, model speed ranking (P50/P95/TTFT), daily usage table |
| Upstream config | Per-upstream enable switches, backend URL overrides, default upstream switch, hot-reload on save |
| Model sync | Pull the real model catalog from upstream catalog APIs (display name / vendor / credit multiplier / context / image support), with daily scheduled sync |
| Throughput speed | **Left tree (upstream → model) + right-side summary of the selected model**: per-turn prefill/decode speed, **cache-hit tokens and percentage**, overall speed trend chart, per-turn detail table |
| One-click check-in | Trae / Buddy check-in, plus check-in-all upstreams; per-run results, daily trend and auto check-in tasks with selectable times |
| Credits dashboard | Trae remaining/total/consumed credits, credit pack details and expiry |
| Logs | View the tail of the latest rolling log file (when `--log-dir` is enabled) |

Corresponding Web APIs (`/admin/api/*`): `overview`, `stats/usage`, `stats/models`, `stats/speed`,
`upstreams`, `upstreams/reload`, `config` (GET/PUT), `models`, `sync`, `checkin/trae`,
`checkin/all`, `checkin/buddy`, `checkin/results`, `credits/trae`, `credits/buddy`, `logs`, `health`.

**Data storage** (config lives in JSON; event records go to **SQLite / SeaORM**):

- Event records (request stats, per-turn throughput, check-in results) all land in
  `<data_dir>/agents2api.db` (SQLite; writes go through a **background single-task serialized
  writer**, queries select raw rows in the window then aggregate in memory; 90-day retention)
- Upstream config lives in `<data_dir>/upstreams.json` (file values override CLI/env)
- The data directory defaults to **`data/` under the program directory** (portable; falls back to
  `%APPDATA%\agents2api` when the program dir isn't writable). Change it with `--data-dir`;
  disable the admin UI with `--no-admin`.

### Frontend development

```bash
cd web
npm install
npm run dev        # Dev mode: /admin proxies to 127.0.0.1:8787, edit frontend and refresh
npm run build      # Emits web/dist, embedded by rust-embed on cargo build
```

The repo already ships a built `web/dist`; backend-only changes don't require rebuilding the frontend.

---

## 🧠 How it works (reverse-engineering notes)

- **CodeBuddy**: reads the local auth `.info` credential → connects directly to
  `copilot.tencent.com/v2/chat/completions` (that backend already speaks standard OpenAI with
  native tools / SSE). Expired tokens are refreshed and atomically written back; supports the
  WorkBuddy 5.6.0+ `$wbEncrypted` envelope (issue #23).
- **Trae**: decrypts the AES-128-CBC login blob inside `storage.json` to get a JWT → calls
  `trae-api-cn.mchost.guru/api/agent/v3/llm_utils_chat` (**ByteDance's private protocol, not
  OpenAI compatible**) → converts private SSE events into OpenAI chunks in real time. The request
  body must be wrapped as `{messages, function, config_name, model, ...}` with `content` as an array.
- **OpenCode (nvidia / modelscope)**: reads the local OpenCode `auth.json` API keys → connects
  directly to each provider's **standard OpenAI-compatible** endpoint (NVIDIA
  `integrate.api.nvidia.com/v1`, ModelScope `api-inference.modelscope.cn/v1`) → injects
  `Authorization: Bearer <key>` and forwards as-is.
- **OpenCode Zen Free (opencode-free)**: no login required → connects directly to the Zen gateway
  `https://opencode.ai/zen/v1` (**standard OpenAI compatible**) → injects the sentinel key
  `Bearer public` and OpenCode CLI identity headers, then forwards as-is; `/models` filters to
  free models whose id contains `-free`.

---

## 🛠 Build

### Requirements

- **Rust 1.75+** (2021 edition; verified on 1.99)
- Windows requires the MSVC toolchain (`stable-x86_64-pc-windows-msvc`)
- No extra system dependencies (HTTP/TLS via rustls, no OpenSSL)

```bash
rustup update stable
```

### Standard build

```bash
git clone <this repo>
cd agents2api

# Dev build (fast, unoptimized, large artifact)
cargo build
# → target/debug/agents2api(.exe)

# Release build (recommended, opt-level=2 + strip)
cargo build --release
# → target/release/agents2api(.exe)  ~7.3 MB
```

The artifact is a **single static binary** with no runtime deps — copy and run:

```bash
# Windows
target\release\agents2api.exe --help

# Linux / macOS
./target/release/agents2api --help
```

### Running tests

```bash
cargo test                       # all 150 tests
cargo test trae                  # Trae-related only
cargo test -- --nocapture        # show output
```

### Lint & format

```bash
cargo check                      # type-check only, faster than build
cargo clippy --all-targets       # static analysis (currently zero warnings)
cargo fmt                        # format
```

Before committing, run in order: `cargo fmt` → `cargo clippy --all-targets` → `cargo test`.

### Packaging

Copy the binary and docs into `dist/` after building:

```bash
# Linux / macOS
mkdir -p dist && cp target/release/agents2api dist/ && cp README.md dist/

# Windows (Git Bash)
mkdir -p dist && cp target/release/agents2api.exe dist/ && cp README.md dist/
```

`dist/` is in `.gitignore` and won't be committed by mistake.

### Cross-compilation

```bash
rustup target add x86_64-unknown-linux-gnu
rustup target add aarch64-apple-darwin
cargo build --release --target x86_64-unknown-linux-gnu
```

> Cross-compiling to Linux/macOS requires the matching linker (e.g. `cross` or `cargo-zigbuild`).
> Building the Windows binary on Windows needs no extra configuration.

### Build artifacts

| Path | Description |
|------|-------------|
| `target/debug/agents2api` | Dev build with debug symbols, fast to compile |
| `target/release/agents2api` | Release build, stripped, small and fast |
| `target/release/build/` | Build-script output (version resource written by winresource) |

### FAQ

**Antivirus false positive**: the built exe may be flagged heuristically (e.g. Huorong reports
`Trojan/Intercept.a`). If the exe disappears right after building, add the project directory to your
antivirus's trust list (see "Antivirus false positives" at the end).

**`Access denied (os error 5)`**: the target exe is running and holds the file. Kill the process
and rebuild:

```bash
taskkill /F /IM agents2api.exe     # Windows
pkill -f agents2api                # Linux / macOS
```

**Slow build**: the first build compiles all dependencies (~1–2 min); incremental builds take
seconds. Use `cargo build --release -j 8` to set parallelism.

---

## ▶️ Run

```bash
# Default 127.0.0.1:8787
agents2api.exe

# Default to Trae; all upstreams are auto-detected and registered
agents2api.exe --upstream trae --api-key mysecret --log converter.log

# Enable only one upstream
agents2api.exe --disable-upstream buddy
```

### CLI flags

| Flag | Description |
|------|-------------|
| `--host` / `--port` | Listen address and port (default `127.0.0.1:8787`) |
| `--api-key` | Optional: API key required from clients |
| `--log PATH` | Single-file log (append, no rotation). Defaults to "directory rotation" mode |
| `--log-dir DIR` | Override the rolling log directory (default "program dir/logs", falls back to data dir/logs) |
| `--no-log` | Console only, no file logging |
| `--log-level LEVEL` | Log level `error` / `warn` / `info` / `debug` (default) / `trace` |
| `--upstream NAME` | Default upstream when no prefix is given |
| `--disable-upstream NAME` | Disable an upstream (repeatable) |
| `--desensitize` / `--no-compact` | Zero-width desensitization (+ whether to compact the harness) |
| `--skip-check` | Skip the startup preflight |
| `--data-dir DIR` | Data directory (SQLite records / admin config), default `data/` under the program dir (falls back to `%APPDATA%\agents2api`) |
| `--no-admin` | Disable the admin UI |
| `--codebuddy-backend URL` | Override the CodeBuddy backend URL |
| `--trae-chat-host URL` | Override the Trae chat host |
| `--trae-storage-json PATH` | Explicit path to Trae storage.json |
| `--opencode-auth-json PATH` | Explicit path to OpenCode auth.json |
| `--opencode-zen-base-url URL` | Override the OpenCode Zen anonymous free channel base URL (default `https://opencode.ai/zen/v1`) |

### Environment variables

| Variable | Description |
|----------|-------------|
| `CODEBUDDY_AUTH_DIR` | CodeBuddy credential directory |
| `CODEBUDDY2OPENAI_KEY` / `CODEBUDDY2OPENAI_LOG` | Same as `--api-key` / `--log` |
| `CODEBUDDY2OPENAI_LOG_DIR` | Same as `--log-dir` |
| `LOG_LEVEL` | Same as `--log-level` (error/warn/info/debug/trace) |
| `AGENTS2API_DATA_DIR` | Same as `--data-dir` |
| `WORKBUDDY_AT_REST_SECRET` / `WORKBUDDY_ELECTRON_PATH` | WorkBuddy at-rest key |
| `TRAE_STORAGE_JSON` | Path to Trae storage.json |
| `TRAE_CHAT_HOST` | Trae upstream host |
| `TRAE_APP_VERSION_CODE` / `TRAE_IDE_VERSION` | Trae version (see below) |
| `TRAE_SALT_A..D` | Override Trae decryption salt (comma-separated decimal bytes) |
| `OPENCODE_AUTH_JSON` | Path to OpenCode auth.json (auto-detected if unset) |
| `OPENCODE_ZEN_BASE_URL` | OpenCode Zen anonymous free channel base URL |
| `OPENCODE_ZEN_TOKEN` | Override the anonymous channel sentinel key (default `public`) |
| `CODEBUDDY_LOSSY_PROJECTION` | `1` = enable lossy projection for /v1/responses |
| `CODEBUDDY_RESPONSES_DESENSITIZE` | `1` = also desensitize /v1/responses |

### Logging

Two file-logging modes, both also writing to the console (stderr):

```bash
# Mode 1: single-file append (no rotation, for short debugging)
agents2api.exe --log converter.log

# Mode 2: date-partitioned directory + rotation (recommended for long runs)
agents2api.exe --log-dir logs
```

`--log-dir` layout:

```
logs/
└── 2026-10-11/
    ├── converter-001.log   ← 100MB max per file, rolls to -002 when full
    └── converter-002.log
```

- Restarting continues the latest file of the day (history is not overwritten)
- Automatically switches to a new date directory across days
- Each line carries a timestamp, level and request id (`[rid]`)

### Log levels

`--log-level` (or `LOG_LEVEL`) controls verbosity, default `debug`:

| Level | Content |
|-------|---------|
| `error` | Errors only: upstream prepare/network failures, non-200, stream interruption |
| `warn` | Adds warnings: no upstream available, catalog persist failure, model catalog sync failure |
| `info` | Adds key runtime events: startup banner, preflight, registry reload, scheduled task triggers |
| `debug` (default) | Adds per-request debugging: request start/end, latency, token counts, stream end |
| `trace` | Adds per-token detail: request body, response body, upstream wire body, raw SSE — **enable only when troubleshooting**, log volume explodes |

```bash
agents2api.exe --log-level debug   # daily use (default)
agents2api.exe --log-level info    # long-running: key events only
agents2api.exe --log-level trace   # troubleshooting upstream protocol issues
```

Log line format: `[2026-10-11 19:18:56] [INFO] ✅ 监听 http://127.0.0.1:8787`

---

## 🔑 Credential locations

**CodeBuddy / WorkBuddy** (`.info` file, preferring `workbuddy-desktop.info`):

- Windows `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\`
- macOS `~/Library/Application Support/CodeBuddyExtension/Data/Public/auth/`
- Linux `~/.local/share/CodeBuddyExtension/Data/Public/auth/`

**Trae** (`storage.json`, auto-detects 4 variants):

- `%APPDATA%\Trae CN\User\globalStorage\storage.json`
- `%APPDATA%\TRAE SOLO CN\User\globalStorage\storage.json`
- `%APPDATA%\Trae\User\globalStorage\storage.json`
- `%APPDATA%\TRAE SOLO\User\globalStorage\storage.json`

The credential is an encrypted blob under the `iCubeAuthInfo://icube.cloudide` key
(`[6B header][32B random][AES-128-CBC]`); the key is derived via `SHA512(SHA512(random) ‖ salt)`.

**OpenCode** (`auth.json`, taking its `nvidia` / `modelscope` entries):

- Windows `%APPDATA%\opencode\auth.json` (also probes `%USERPROFILE%\.local\share\opencode\`)
- macOS / Linux `~/.local/share/opencode/auth.json` (also probes `~/.config/opencode/`)

---

## 🅃 Trae support status

The Trae upstream is a **reverse-engineered private protocol** (referencing the
[smart-open/TraeWorkAssistant](https://github.com/smart-open/TraeWorkAssistant) implementation
verified on real machines). Current status:

- ✅ **Works end-to-end**: login decryption, auth, chat, streaming, tool calls and thinking all
  verified on real hardware
- ✅ Verified models: `deepseek-v4-flash`, `deepseek-v4-pro`, `glm-5.3`, `glm-5.2`, `kimi-k3`, etc.
- ✅ Supports `reasoning_content` (chain of thought), `tool_calls` (with incremental chunk merging)
  and usage

### Protocol notes (pitfalls hit along the way)

These differ from the original project's early implementation and were fixed only after real-machine
verification:

| Item | Correct approach |
|------|------------------|
| Auth header | **`x-ide-token: <raw JWT>`**, do not send `Authorization`. The `Cloud-IDE-JWT ` prefix is rejected by the gateway before token validation (401 / code 1001) |
| Version | In the **header** (`x-app-version-code` / `x-ide-version-code` = `20260811`), not the body |
| Required body fields | `prompt_max_tokens: 168000` (missing or 1M both return 4001), `model_name` (`<config>__dev`), `user_id`, `device_id`, `machine_id`, `workspace_id`, `mode: FunctionCall`, etc. |
| Model mapping | Dual fields: `config_name` (display) + `model_name` (internal); e.g. `deepseek-v4-flash` → `DeepSeek-V4-Flash` / `deepseek_v4_flash__dev` |
| Function view | Most models use `solo_work_lite`; `deepseek-v4.1-flash`, `doubao-seed-code`, `glm-5.3-flash`, `qwen3.8-flash`, `kimi-k2.8-preview` are only available under `solo_agent` |
| SSE event name | Taken **only from the `event:` line**; the payload's `type` field is not used |
| SSE text field | Use **`response`** (not `content`); chain of thought is in `reasoning_content` |
| Tool calls | `function_call` sub-object; **one call is split into multiple incremental chunks** (first chunk gives id+name, subsequent chunks only arguments), must be accumulated by index |
| Error code | `4001` (param invalid / model config mismatch) is a **request-level** error, unrelated to the account — pass through 400 directly |

If the upstream returns `4001`, troubleshoot in order: ① is `prompt_max_tokens` 168000;
② is the model name spelled correctly; ③ does the model require the `solo_agent` view.

Start with `--log`; the log prints the "WIRE BODY sent to upstream" and "TRAE RAW SSE" for comparison.

### Environment variables

| Variable | Description |
|----------|-------------|
| `TRAE_CHAT_HOST` | Upstream host (default `https://trae-api-cn.mchost.guru`) |
| `TRAE_IDE_VERSION` / `TRAE_IDE_VERSION_CODE` | Client version / version code (default `0.1.50` / `20260811`) |
| `TRAE_APP_ID` | App ID |
| `TRAE_STORAGE_JSON` | Explicit path to storage.json |
| `TRAE_SALT_A..D` | Override decryption salt (comma-separated decimal bytes) |

---

## 🧪 Tests

```bash
cargo test            # all 150 tests
cargo test trae       # Trae-related only
```

Coverage:

- **Protocol adapters**: Anthropic / Responses request conversion and streaming event conversion
  (tool_calls chunk merging, usage merging, cached_tokens fallback chain, sequence_number
  continuity, upstream errors must not become empty success, incomplete output)
- **Trae upstream**: storage.json decryption round-trip and tamper detection, private event mapping
  (including the observed `event:` line format, split tool_calls full-chain aggregation regression),
  request headers and required fields, model mapping and function view selection, error code mapping
- **Multi-upstream**: path prefix routing dispatch, per-upstream isolated model lists, unknown
  upstream 404
- **Record persistence**: SQLite writes and windowed queries, accounting even on client
  interruption (Drop finalizer guard), idempotency
- **Misc**: identity declaration filtering, zero-width desensitization, Responses projection
  compaction, CodeBuddy encryption round-trip

---

## 🔍 Common search keywords

> If you found this project via a search engine, these terms all point to what this project does:

`AI gateway` · `LLM proxy` · `OpenAI API compatible` · `OpenAI API proxy` ·
`Claude API compatible` · `Anthropic API proxy` · `OpenAI to Anthropic` ·
`Claude Code setup` · `Codex CLI config` · `Cursor custom model` · `Cline setup` ·
`Roo Code model` · `CC Switch` · `AI coding tool` · `AI coding agent` ·
`local LLM API` · `free LLM API` · `LLM API conversion` · `DeepSeek API proxy` ·
`Kimi API` · `GLM API` · `Qwen API` · `Trae API` · `CodeBuddy API` ·
`OpenRouter alternative` · `one-api alternative` · `new-api` · `Rust API gateway`

---

## ⚠️ Antivirus false positives

The program's behavioral traits (reading credentials, injecting Authorization headers, decrypting
login state, zero-width character desensitization) are easily flagged by antivirus heuristics
(e.g. Huorong `Trojan/Intercept.a`). The source is fully open — audit and compile it yourself, and
add the project directory to your trust list.

## License

MIT (same as the original project)

---

**[⬆ Back to top](#agents2api)** · [Original project codebuddy2api](https://github.com/ShouZhuo0413/codebuddy2api) (Python / FastAPI), rewritten in Rust · ⭐ If this project helps you, a **Star** is appreciated
