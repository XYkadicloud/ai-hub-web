# AI Hub Web · 交接文档

> 版本：v0.1.0　｜　最后更新：2026-10-23
> 定位：轻量级「一站式 AI 聊天客户端」，本地 HTTP 服务 + 系统默认浏览器，零 WebView2 依赖。

---

## 一、项目是什么

一个跑在 Windows 上的单文件程序（约 5MB）。双击后：

1. 在本机 `127.0.0.1` 的**随机端口**起一个 HTTP 服务；
2. 自动用**系统默认浏览器**打开界面（Edge / Chrome 都行）；
3. 界面是 ChatGPT 风格的暗色聊天 UI，左侧会话列表、流式打字机、Markdown 渲染。

**设计目标**：弱机可用（4GB 内存 / Intel N3060 级别），不需要 WebView2、Node、Python。

**前端来源**：前端 HTML/CSS/JS 与早期 Tauri 桌面版**完全共用同一份代码**。
浏览器版通过 `web/vendor/tauri-shim.js` 在浏览器里伪造了 `window.__TAURI__` 同名 API，
所以前端一行未改即可运行。这是本项目最关键的设计取巧点。

---

## 二、当前能做什么 / 不能做什么

### 已实现并验证

| 能力 | 状态 | 验证方式 |
|---|---|---|
| OpenAI 兼容协议（DeepSeek / 中转站 / 自建反代） | ✅ | mock 服务端到端 + 真实 HTTPS 探测 |
| Gemini **原生**协议（非兼容层） | ✅ | mock 原生协议端到端 |
| ChatGPT 网页版协议（含 PoW 求解） | ✅ 代码完成 | 协议级 mock（含服务端 PoW 校验） |
| Claude 网页版协议（sessionKey） | ✅ 代码完成 | 协议级 mock |
| 账号池 + 自动故障转移（401/403/429/额度） | ✅ | mock 触发切换 |
| 多轮上下文（含网页版会话/父消息 ID 持久化） | ✅ | 两协议第二轮测试 |
| 会话历史本地持久化 | ✅ | 跨重启读取 |
| 流式输出（SSE） | ✅ | 逐字增量推送 |
| 浏览器 UI（设置、账号池、抓取弹窗、Key 表单） | ✅ | Playwright 真实浏览器，零 JS 错误 |
| Windows 交叉编译 | ✅ | `x86_64-pc-windows-gnu`，仅依赖系统 DLL |
| **生图**（🎨 开关 + 模型名自动识别） | ✅ 代码完成 | `/images/generations` → base64 落盘 → SSE 推 Markdown 图片 |
| **文件上传**（📎 + 拖拽，图片/文本/文件） | ✅ 代码完成 | 原始字节 POST `/api/upload`；图片走多模态，文本内联 |
| **ChatGPT OAuth 登录**（PKCE + refresh 自动续期） | ✅ 代码完成 | 授权码流程按 `chatgpt2api` 同规格实现；真机待验证 |
| **书签脚本零粘贴抓取** | ✅ 代码完成 | 页面上下文取 accessToken → CORS POST 回本地服务 → SSE 自动收尾 |
| **流式取消** | ✅ | `POST /api/chat/stop` + 各协议流式循环检查 |
| **接入方式切换 UI** | ✅ | 每张服务卡下拉切换 网页版 / OpenAI 兼容 / Gemini 原生 |
| **固定端口 25880**（可 `--port` 覆盖，占用回退随机） | ✅ | 书签脚本/OAuth 依赖端口稳定 |

### 明确未做 / 有局限

| 项 | 说明 |
|---|---|
| **ChatGPT / Claude 真实账号实测** | 开发环境无登录态，只用「严格复刻官方协议的 mock」验证。真机上若失效，大概率是官方改了接口，不是代码逻辑错 |
| **OAuth 真机登录** | 按 `yukkcat/chatgpt2api` 的公开流程实现（authorize/code 兑换/refresh），首次真机使用若报错，优先核对 `state` 与回调 URL 完整性 |
| **Gemini 国内直连** | `generativelanguage.googleapis.com` 国内需代理，无代理不可用 |
| **ChatGPT / Claude 网页版** | 同样需要能访问对应站点（需代理） |
| **语音识别** | 产品决策：不做（浏览器原生/后端 whisper 均不实现） |
| **Gemini 原生生图** | 生图分支只走 OpenAI 兼容 `/images/generations`；Gemini 图片模型（imagen 等）未接 |
| **安装包** | 目前只产出裸 exe，无安装器、无开机自启、无托盘 |

---

## 三、技术栈

| 层 | 选型 | 说明 |
|---|---|---|
| 语言 | Rust 2021 | 单二进制、无运行时 |
| HTTP 服务 | axum 0.7 | 注意：路由参数是 `:id` 语法，不是 0.8 的 `{id}` |
| 异步 | tokio（`rt-multi-thread`, 2 worker） | 弱机友好 |
| HTTP 客户端 | reqwest 0.12 + rustls | **不用 OpenSSL**，交叉编译免依赖 |
| 静态资源 | rust-embed 8 | 编译期内嵌，也可 `--web-dir` 外置 |
| 推送 | tokio broadcast + SSE | 单用户场景足够，未用 WebSocket |
| 哈希 | sha3 0.10 | ChatGPT PoW 用 SHA3-512 |
| 序列化 | serde / serde_json | 配置与会话均为 JSON 文件 |

体积优化（`Cargo.toml`）：`panic="abort"`、`strip=true`、`opt-level="s"`、`codegen-units=1`。

---

## 四、目录结构

```
ai-hub-web/
├── Cargo.toml
├── build.rs                  # 注入构建时间 BUILD_TS，供界面显示版本
├── README.md                 # 用户向说明
├── HANDOVER.md               # 本文档（开发向）
├── .github/workflows/build.yml  # CI：test + Windows/Linux 双平台 release 构建
├── src/
│   ├── main.rs       (150)   # 入口：数据目录、固定端口、开浏览器、优雅退出、--web-dir/--port
│   ├── server.rs     (760)   # axum 路由 / REST / SSE / 静态资源 / 上传 / OAuth / 中止
│   ├── chat.rs       (350)   # 按 ptype 分发 + OpenAI 兼容 SSE + 生图分支 + 账号故障转移
│   ├── webchat.rs    (940)   # ChatGPT(PoW) / Claude(sessionKey) 两个网页协议 + OAuth 续期
│   ├── gemini.rs     (410)   # Gemini 原生协议（streamGenerateContent，支持图片 parts）
│   ├── oauth.rs      (260)   # ChatGPT 官方 OAuth：PKCE、code 兑换、refresh 续期、JWT 解析
│   ├── chat_attach.rs(180)   # 附件模型 + 多模态内容组装（OpenAI 数组 / Gemini inline_data）
│   ├── files.rs      (200)   # 上传/生图落盘、安全寻址、附件分类
│   ├── config.rs     (345)   # 配置与账号池、默认预置、迁移逻辑
│   ├── history.rs    (175)   # 会话持久化（含 meta 与 attachments）
│   └── providers.rs  (21)    # URL 拼接 / HTTP Client 构造（含代理）
└── web/
    ├── index.html    (155)
    ├── style.css     (400)
    ├── app.js        (1180)
    └── vendor/
        ├── tauri-shim.js   (215)  # 浏览器版核心：伪造 window.__TAURI__ + uploadFile
        └── marked.min.js          # Markdown 渲染
```

代码总量：Rust 约 2737 行，前端约 1579 行。

---

## 五、架构与数据流

```
浏览器 (app.js)
   │  ① invoke("chat-stream", ...)  →  POST /api/chat
   │  ② new EventSource("/api/events")
   ▼
tauri-shim.js  ──把 __TAURI__.core.invoke 翻译成 fetch──┐
                                                        ▼
                                              axum server (server.rs)
                                                        │
                                    ┌───────────────────┴───────────────────┐
                                    │ chat::stream_chat                     │
                                    │  按 provider.ptype 分发：              │
                                    │   openai        → chat.rs 内置 SSE     │
                                    │   gemini_native → gemini.rs            │
                                    │   chatgpt_web   → webchat.rs           │
                                    │   claude_web    → webchat.rs           │
                                    └───────────────────┬───────────────────┘
                                                        │ ChatEvent
                                                        ▼
                                    桥接任务：ChatEvent → ServerEvent
                                                        │ broadcast
                                                        ▼
                                             GET /api/events (SSE)
                                                        │
                                                        ▼
                                            浏览器 EventSource 渲染
```

**关键设计：请求 / 推送分离。**
`POST /api/chat` 只负责启动任务并立刻返回；真正的流式内容通过全局 broadcast 频道 → SSE 推给页面。
好处：前端只需一条长连接，多标签页也能同时收到。

**ChatEvent 枚举**（`chat.rs`）：
`Delta(String)` / `Switch{account,reason}` / `Active{account}` / `Done` / `Error(String)`

**ServerEvent**（`server.rs`）：`{ event: String, payload: Value }`，
经 SSE 下发后由 shim 派发为 `listen(event, cb)` 回调。

---

## 六、四种协议实现细节

### 6.1 `openai` —— OpenAI 兼容

- 端点：`{base_url}/chat/completions`
- 鉴权：`Authorization: Bearer {secret}`
- 请求体：标准 `{model, messages, stream:true}`
- 流式：`data: {"choices":[{"delta":{"content":"..."}}]}`，`[DONE]` 结束
- 适用：DeepSeek 官方、任意中转站、自建反代、Gemini 兼容层（可选）

### 6.2 `gemini_native` —— Gemini 原生协议

- 端点：`{base}/models/{model}:streamGenerateContent?alt=sse`
  （`base` 默认 `https://generativelanguage.googleapis.com/v1beta`）
- 鉴权：请求头 `x-goog-api-key: {secret}`（**不是** Bearer）
- 请求体：
  ```json
  {
    "contents": [{"role":"user","parts":[{"text":"..."}]}],
    "systemInstruction": {"parts":[{"text":"..."}]},
    "generationConfig": {...}
  }
  ```
  注意角色映射：`assistant` → `model`
- 流式解析：`candidates[0].content.parts[].text`
  **关键**：跳过 `"thought": true` 的 part（思维链不展示给用户）
- 错误处理：`classify_error()` 把 400/403/429 翻译成中文提示
- 模型列表：`GET {base}/models`，只保留支持 `generateContent` 的

### 6.3 `chatgpt_web` —— ChatGPT 网页版

账号字段：`secret` = **accessToken（JWT）**，`cookie` = 可选完整 Cookie。

流程（`webchat.rs::chatgpt_send`）：

1. **取 dpl**：GET `https://chatgpt.com/`，用正则 `c/[^/]*/_` 从 script src 里提取部署指纹；
   失败则回退 `DEFAULT_DPL` 常量。
2. **chat-requirements**：POST `/backend-api/sentinel/chat-requirements`
   → 返回 `{token, proofofwork: {required, seed, difficulty}}`
3. **PoW 求解**（`solve_pow`）：
   - 构造 JSON 配置（含 `seed`、`difficulty`、`nonce`），base64 编码；
   - 计算 `SHA3-512(seed_bytes ‖ base64_bytes)`；
   - 要求摘要前 N 字节 ≤ `difficulty`（十六进制解码后逐字节比较）；
   - 命中即返回该 base64 串作为 proof。
   - 实现细节：零分配内循环 + 手写 u64 → ASCII 转换（`push_u64`），
     **release 下 `000032` 难度约 1.02 秒**；debug 下约 129 秒（正常，勿用 debug 测）。
4. **发消息**：POST `/backend-api/conversation`，头包括：
   - `Authorization: Bearer {accessToken}`
   - `openai-sentinel-chat-requirements-token: {token}`
   - `openai-sentinel-proof-token: {proof}`
   - `User-Agent`（桌面 Chrome）、`Cookie`（若有）
   - 请求体：`{action:"next", messages:[...], model, conversation_id?, parent_message_id}`
5. **流式解析**（`handle_chatgpt_event`）：按 `message.content.parts` 增量取正文，
   `status == "in_progress"` 时视为中间态；`[DONE]` 结束。
6. **多轮**：会话 `meta` 里保存 `conversation_id` 与 `parent_message_id`，下轮带上。

**已知门槛**：Cloudflare 403 / Turnstile / Arkose 人机验证可能间歇性出现，
出现时本实现会报错并由账号池切下一个账号，不做自动过验证。

**OAuth 登录（新增，推荐）**：账号也可通过官方 OAuth（PKCE 授权码）获得——
`src/oauth.rs` 按社区项目 `yukkcat/chatgpt2api` 的公开流程实现：

1. `POST /api/oauth/start` 生成 `code_verifier`/`state`，返回 authorize URL；
2. 用户在系统浏览器登录，回调落在 `platform.openai.com/auth/callback?code=...`；
3. `POST /api/oauth/finish` 用 code 换 `access_token` + `refresh_token` + `id_token`，
   JWT 解析出 email / `chatgpt_account_id` 存入账号 meta；
4. 之后每次对话前检查 AT 临期（<5 分钟）即用 refresh_token 自动续期并写回账号池。

授权参数：`client_id = app_2SKx67EdpoN0G6j64rFvigXD`（公开客户端，无 secret），
scope `openid profile email offline_access`，兑换端点 `auth.openai.com/api/accounts/oauth/token`，
续期端点 `auth.openai.com/oauth/token`（form-urlencoded）。

### 6.4 `claude_web` —— Claude 网页版

账号字段：`secret` = **sessionKey**（Cookie 值，HttpOnly，JS 读不到，需手动复制）。

流程（`webchat.rs::claude_send`）：

1. **取组织**：GET `/api/account` → `memberships[0].organization.uuid`
2. **建会话**：POST `/api/organizations/{org}/chat_conversations`
   body `{"name":"", "model": "..."}` → 返回 `uuid`
3. **对话**：POST `/api/organizations/{org}/chat_conversations/{uuid}/completion`
   body 含 `prompt`、`timezone`、`personalized_styles`、`tools`
4. **流式**：SSE 事件 `content_block_delta` → `delta.text`；`message_stop` 结束
5. **多轮**：`meta` 保存 `claude_conv_uuid` 与 `parent_uuid`，续写时带 `parent_message_uuid`
6. **限流**：429 时读 `anthropic-ratelimit-requests-reset` 头

请求头含 `anthropic-client-version`（常量 `2025-04-01`）与桌面 UA。

> ⚠️ 上述两个网页协议的接口细节来自开源项目 `lanqian528/chat2api` 与 `caiwuu/web2api` 的源码分析。
> 官方随时可能变更，失效时请以最新源码为准。

---

## 七、HTTP API 清单

所有接口只监听 `127.0.0.1`，无鉴权（单用户本机应用）。

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/` | 返回 index.html |
| GET | `/api/config` | 读取配置（含 providers 与 accounts） |
| POST | `/api/config` | 整体保存配置 |
| GET | `/api/accounts?provider_id=` | 列出某服务的账号 |
| POST | `/api/accounts` | 新增账号（body 为 `{provider_id, account}`） |
| POST | `/api/accounts/status` | 启用 / 停用账号 |
| DELETE | `/api/accounts/:id?provider_id=` | 删除账号 |
| POST | `/api/capture-login-token` | 网页登录抓取的 Token 入库（书签脚本跨域 POST 目标，全局 CORS 已放开） |
| POST | `/api/verify-account` | **添加前真实校验**：Gemini 走 `models`，ChatGPT 走 session 接口，Claude 走 `/api/account` |
| POST | `/api/models` | 拉取可用模型列表 |
| GET | `/api/version` | 版本 + 构建时间（界面底部展示，用于核对 exe 新旧） |
| POST | `/api/chat` | 发起对话（body: provider_id / model / messages / conv_id / image_mode） |
| POST | `/api/chat/stop` | 停止某会话的流式输出（body: `{conv_id}`） |
| POST | `/api/upload` | 原始字节上传附件（query: name/mime，body 为字节） |
| GET | `/api/files/:id` | 读取上传的附件文件 |
| GET | `/api/images/:id` | 读取生图结果（png） |
| POST | `/api/oauth/start` | ChatGPT OAuth 第一步：生成 PKCE + authorize URL |
| POST | `/api/oauth/finish` | ChatGPT OAuth 第二步：code 换 token，落账号池 |
| GET | `/api/conversations` | 会话列表 |
| POST | `/api/conversations` | 新建会话 |
| GET | `/api/conversations/:id` | 读取会话（含 messages、meta） |
| DELETE | `/api/conversations/:id` | 删除会话 |
| POST | `/api/conversations/:id/rename` | 重命名 |
| POST | `/api/conversations/:id/messages` | 追加一条消息（body 支持 `attachments`） |
| GET | `/api/events` | **SSE 长连接**，推送 `chat_stream` / `account_ok` / `account_switch` / `login-captured` 等 |
| POST | `/api/quit` | 优雅退出（页面关闭时 sendBeacon 调用） |
| GET | `/*path` | 静态资源兜底（内嵌或 `--web-dir`） |

> 所有接口监听 `127.0.0.1`，无鉴权（单用户本机应用）。响应统一带 CORS 头，
> 这是书签脚本能从 `chatgpt.com` 页面跨域回传 Token 的前提。

**前端 → 后端命令映射**（`tauri-shim.js`）：
`get-config` / `save-config` / `get-accounts` / `save-account` / `delete-account` /
`set-account-status` / `capture-login-token` / `verify-account` / `chat-stream` / `chat-stop` /
`oauth-start` / `oauth-finish` / `list-conversations` / `create-conversation` / `get-conversation` /
`delete-conversation` / `rename-conversation` / `save-message`；
另暴露非 invoke 助手 `TAURI.core.uploadFile(name, mime, bytes)`（原始字节上传）。

---

## 八、数据存储

位置：Windows `%APPDATA%\ai-hub\`，Linux `~/.ai-hub/`。

```
ai-hub/
├── config.json           # 服务 + 账号池
├── startup.log           # 每次启动记录实际端口（排障用）
├── uploads/              # 聊天附件（图片/文本/文件，内容哈希命名）
├── images/               # 生图结果（png，内容哈希命名）
└── conversations/
    └── {conv_id}.json    # 单个会话，含 messages 与 meta
```

### config.json 关键结构

```json
{
  "providers": [{
    "id": "deepseek",
    "name": "DeepSeek",
    "ptype": "openai",              // openai | gemini_native | chatgpt_web | claude_web
    "base_url": "https://api.deepseek.com/v1",
    "models": ["deepseek-chat"],
    "default_model": "deepseek-chat",
    "proxy": null,
    "enabled": true,
    "accounts": [{
      "id": "…",
      "name": "我的账号",
      "cred_type": "api_key",       // api_key | token
      "secret": "sk-…",             // API Key / accessToken / sessionKey
      "cookie": null,               // ChatGPT 网页版用
      "meta": null,                 // Claude 的 org_uuid 等
      "status": "active",           // active | disabled | error | expired
      "priority": 0
    }]
  }]
}
```

### 会话 meta（网页协议多轮状态）

```json
{
  "conversation_id": "…",       // ChatGPT
  "parent_message_id": "…",     // ChatGPT
  "claude_conv_uuid": "…",      // Claude
  "parent_uuid": "…"            // Claude
}
```

### 账号选择与故障转移

`config::resolve_accounts()` 按 `priority` 升序排列可用账号；
`mark_account_error()` 在失败时标记并降级，下一轮自动跳过。
触发切换的条件：401 / 403 / 429 / 额度耗尽 / 明确的鉴权错误。

---

## 九、构建

### 本机（Linux）

```bash
cargo build --release
./target/release/ai-hub-web
```

### 交叉编译 Windows

```bash
rustup target add x86_64-pc-windows-gnu
# Ubuntu 需：apt install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
# 产物：target/x86_64-pc-windows-gnu/release/ai-hub-web.exe
```

产物特性：约 4.9–5.1MB，仅 import `kernel32` / `ws2_32` / `bcrypt` 等系统 DLL，
**无 OpenSSL、无 WebView2、无 libgcc 外部依赖**。

### GitHub Actions（推荐，免本地环境）

`.github/workflows/build.yml`：push 到 main 或手动 dispatch 时，
在 windows-latest 跑 `cargo test --release` + `cargo build --release`，
ubuntu-latest 出 Linux 二进制，产物以 artifact 形式上传（`ai-hub-web-windows-x86_64` 内含 exe）。

### 运行时参数

```bash
ai-hub-web.exe                 # 默认：内嵌前端 + 固定端口 25880 + 自动开浏览器
ai-hub-web.exe --port 9000     # 指定端口（也可用环境变量 AI_HUB_PORT）
ai-hub-web.exe --web-dir ./web # 从磁盘读前端，改完刷新即可生效（开发用）
```

端口策略：**固定端口优先**（书签脚本回传、OAuth 回调都依赖端口稳定），
被占用时自动回退随机端口并记入 `startup.log`。

### 环境变量

`AI_HUB_NO_OPEN=1` 可阻止自动打开浏览器（CI / 调试用）。

---

## 十、测试

### 已建立的测试手段

| 类型 | 做法 |
|---|---|
| 单元测试 | `cargo test --release pow_solves` 验证 PoW 正确性与耗时 |
| 协议级 mock | Python 起 mock 服务，严格复刻 ChatGPT sentinel+PoW、Claude SSE、Gemini 原生 SSE |
| API 级 | Python 脚本走 REST + SSE，校验逐字增量与事件序列 |
| UI 级 | Playwright 真实 Chromium，检查 DOM、弹窗、零 JS 错误、截图 |

### 关键验证结论

- ChatGPT PoW（`000032`）：release **1.02s**，debug 129s
- 两网页协议多轮：第二轮正确携带会话/父消息 ID
- 真实 HTTPS：指向 Google 返回标准 `API key not valid`，证明 rustls TLS 正常
- 浏览器端：四个服务按钮文案与弹窗路由正确，零 JS 错误

### 复现测试的建议顺序

1. `cargo test --release pow_solves` —— 先确认 PoW 没坏
2. 起 mock → 改 `base_url` 指向 mock → 走完整 chat
3. Playwright 跑 UI 回归（注意用 `domcontentloaded`，**不要用 `networkidle`**，
   因为 SSE 长连接会让网络永不空闲）

---

## 十一、已知问题（重要，按优先级）

### 1. ~~缺少「接入方式」切换 UI~~ —— 已修复

每张服务卡现有「接入方式」下拉：ChatGPT / Claude 可切「网页版 ⇄ OpenAI 兼容（反代）」，
Gemini 可切「官方原生 ⇄ OpenAI 兼容层」，切换时联动字段标签与账号池按钮。

### 2. ~~账号池两个按钮对网页版端点行为相同~~ —— 已修复

`renderAccounts()` 现按 `ptype` 只显示一个按钮：网页版端点只显示「网页登录抓 Token」，
Key 类端点只显示「＋ 手动填 Key」。

### 3. 网页版协议无真实账号验证

仅通过 mock 验证。若真机失效，排查顺序：
dpl 提取 → sentinel 接口字段 → PoW 难度算法 → conversation 请求体字段。

### 4. ~~流式取消未实现~~ —— 已实现

`POST /api/chat/stop {conv_id}` 置位中止标志；chat/gemini/webchat 三条流式循环
每个 chunk 前检查标志，收到后立即收尾（无输出时回 `已停止` 错误）。

### 5. ~~遗留死代码~~ —— 已清理

`/api/open-login`、`open_login` 处理器已删除；shim 里的 `open-login-window` /
`close-login-window` 仍保留（Tauri 版兼容，无副作用）。

### 6. debug 构建下 PoW 极慢

`cargo build`（debug）跑 ChatGPT 会卡 2 分钟以上，属正常现象。
**测试与交付一律用 `--release`。**

### 7. 单用户假设

无鉴权、无多用户隔离、broadcast 频道全量推送。
若要多开或局域网访问，需重新设计鉴权与事件路由。

### 8. 生图仅覆盖 OpenAI 兼容端点

`/images/generations` 分支只对 `openai` 类型 provider 生效；
Gemini 原生图片模型（imagen 等）与网页版协议内的生图均未接。

---

## 十二、后续待办建议

按性价比排序：

1. **OAuth / 网页版真实账号回归测试** —— 有 ChatGPT 账号后跑一遍 OAuth 登录与续期，记录失效点
2. **Gemini 原生生图**（imagen / 图片预览模型走 `generateContent`）
3. **ChatGPT 网页版生图**（`/backend-api/files` 上传 + 多模态消息）
4. **Claude 官方 API 协议**（`/v1/messages` + `x-api-key`），比网页版稳定
5. **安装包**：NSIS / MSI，带开始菜单快捷方式与卸载
6. **托盘 + 单实例**：避免重复启动起多个服务（固定端口已部分缓解）
7. **会话搜索 / 导出 Markdown**
8. **代理配置细化**：目前是整服务一个 proxy，可细化到「仅网页版走代理」

---

## 十三、合规与风险声明

- 本项目是**可配置端点的客户端**：不内置、不分发任何 API Key 或共享账号。
- `chatgpt_web` / `claude_web` 通过**用户自己的登录态**访问网页接口，
  这类用法**可能违反对应服务的条款**，存在封号风险，由使用者自行承担。
- 逆向接口随官方更新随时可能失效，**不承诺长期可用**。
- 需要长期稳定，请优先使用官方 API（DeepSeek / Gemini 官方 Key）或自建合规反代。
- 所有数据仅存本机，不上传任何服务器。

---

## 十四、快速上手（接手者 5 分钟）

```bash
git clone <repo>
cd ai-hub-web
cargo build --release
./target/release/ai-hub-web          # 浏览器自动打开（固定端口 25880）
```

1. 点左下角「⚙ 设置 / 账号池」
2. 标签「账号池」→ 选 DeepSeek → 「＋ 添加 API Key」→ 粘贴官方 Key → 验证并添加
3. 关弹窗 → 顶部选 DeepSeek → 输入框打字 → Enter

想看前端改动效果，用 `--web-dir ./web`，改完刷新浏览器即可，不必重编译。
