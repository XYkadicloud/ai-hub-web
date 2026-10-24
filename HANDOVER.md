# AI Hub Web · 交接文档

> 版本：v0.2.0　｜　最后更新：2026-10-24
> 定位：轻量级「一站式 AI 聊天客户端」，本地 HTTP 服务 + 系统默认浏览器，零 WebView2 依赖。
> 本版新增：**Agent 模式（模型自主调用工具，仅 API 端点）**、上传机制重做（PDF/Office 文本提取）、
> 删除 Claude 网页版（改用 Claude API，openai 协议，默认关闭）。

---

## 一、项目是什么

一个跑在 Windows 上的单文件程序（约 5MB）。双击后：

1. 在本机 `127.0.0.1:25880`（**固定端口**，被占用才回退随机）起一个 HTTP 服务；
2. 自动用**系统默认浏览器**打开界面（Edge / Chrome 都行）；
3. 界面是 ChatGPT 风格的暗色聊天 UI，左侧会话列表、流式打字机、Markdown 渲染；
4. 页面关闭后服务端自动退出（前端 5s 心跳 + `pagehide` 时 `sendBeacon /api/quit` 设 8s 死线）。

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
| ChatGPT 网页版协议（含 PoW 求解） | ✅ **真机跑通** | 真实 OAuth 账号对话 + 多轮上下文 |
| ~~Claude 网页版协议~~ | ❌ **已删除** | 账号 429 限流 + 封号风险；改为 Claude API（openai 协议，默认关闭） |
| **Agent 模式**（模型自主调用工具） | ✅ **真机跑通** | Gemini 原生协议：时间/计算双工具、联网搜索串抓网页 |
| **联网搜索工具**（`web_search` + `fetch_url`） | ✅ | 真机：搜索 Rust 版本 → 抓 releases.rs → 答 1.99.1 |
| 账号池 + 自动故障转移（401/403/429/额度） | ✅ | mock 触发切换 |
| 多轮上下文（含网页版会话/父消息 ID 持久化） | ✅ | 两协议第二轮测试 |
| 会话历史本地持久化 | ✅ | 跨重启读取 |
| 流式输出（SSE） | ✅ | 逐字增量推送 |
| 浏览器 UI（设置、账号池、抓取弹窗、Key 表单、Agent 开关） | ✅ | Playwright 真实浏览器，零 JS 错误 |
| Windows 交叉编译 | ✅ | `x86_64-pc-windows-gnu`，仅依赖系统 DLL |
| **生图**（🎨 开关 + 模型名自动识别） | ✅ 代码完成 | `/images/generations` → base64 落盘 → SSE 推 Markdown 图片 |
| **文件上传**（📎 + 拖拽，图片/文本/文件） | ✅ **已修复重做** | 扩展名推断 + MIME 补全 + PDF/Office 文本提取 |
| **ChatGPT OAuth 登录**（PKCE + refresh 自动续期） | ✅ 代码完成 | 授权码流程按 `chatgpt2api` 同规格实现；真机待验证 |
| **书签脚本零粘贴抓取** | ✅ 代码完成 | 页面上下文取 accessToken → CORS POST 回本地服务 → SSE 自动收尾 |
| **流式取消** | ✅ | `POST /api/chat/stop` + 各协议流式循环检查 |
| **接入方式切换 UI** | ✅ | 每张服务卡下拉切换 网页版 / OpenAI 兼容 / Gemini 原生 |
| **固定端口 25880**（可 `--port` 覆盖，占用回退随机） | ✅ | 书签脚本/OAuth 依赖端口稳定 |

### 明确未做 / 有局限

| 项 | 说明 |
|---|---|
| **ChatGPT 真机实测** | 已跑通（OAuth 账号，单轮 + 多轮） |
| **Agent 的 OpenAI 兼容协议真机验证** | 代码路径已实现（`agent.rs::run_openai_agent`），因无可用 Key 未实测；Gemini 原生协议已真机跑通。拿到 Key 后跑 `verify-agent.cjs openai` 即可 |
| **OAuth 真机登录** | 已跑通（Codex loopback 全自动）。若报错，优先核对 `state`、端口是否被防火墙拦 |
| **Gemini 国内直连** | `generativelanguage.googleapis.com` 国内需代理，无代理不可用 |
| **ChatGPT 网页版** | 需要能访问 chatgpt.com（需代理） |
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
│   ├── main.rs       (208)   # 入口：数据目录、固定端口、开浏览器、优雅退出、--web-dir/--port
│   ├── server.rs     (1162)  # axum 路由 / REST / SSE / 静态资源 / 上传 / OAuth / 中止
│   ├── chat.rs       (487)   # 按 ptype 分发 + OpenAI 兼容 SSE + 生图分支 + 账号故障转移 + Agent 入口
│   ├── agent.rs      (668)   # Agent 循环：OpenAI 工具调用 / Gemini functionCall 两协议实现
│   ├── agent_tools.rs(602)   # Agent 四个内置工具：时间 / 计算 / 联网搜索 / 抓网页
│   ├── webchat.rs    (1036)  # ChatGPT 网页协议（PoW + OAuth 续期）；Claude 已删除
│   ├── gemini.rs     (414)   # Gemini 原生协议（streamGenerateContent，支持图片 parts）
│   ├── oauth.rs      (477)   # ChatGPT 官方 OAuth：PKCE、code 兑换、refresh 续期、JWT 解析
│   ├── chat_attach.rs(219)   # 附件模型 + 多模态内容组装（OpenAI 数组 / Gemini inline_data）
│   ├── docext.rs     (366)   # 文档文本提取：PDF（FlateDecode + 字符串对象）/ Office（zip + XML 剥标签）
│   ├── files.rs      (214)   # 上传/生图落盘、安全寻址、附件分类
│   ├── config.rs     (389)   # 配置与账号池、默认预置、迁移逻辑
│   ├── history.rs    (178)   # 会话持久化（含 meta 与 attachments）
│   └── providers.rs  (137)   # URL 拼接 / HTTP Client 构造（含代理）
└── web/
    ├── index.html    (195)
    ├── style.css     (458)
    ├── app.js        (1406)
    └── vendor/
        ├── tauri-shim.js   (249)  # 浏览器版核心：伪造 window.__TAURI__ + uploadFile
        └── marked.min.js          # Markdown 渲染
```

代码总量：Rust 约 6387 行，前端约 2308 行。

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
                                    │   agent_mode && ptype != chatgpt_web    │
                                    │     → agent.rs（模型自主调工具）       │
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
`Delta(String)` / `Switch{account,reason}` / `Active{account}` / `Done` / `Error(String)` /
**`ToolCall{name, args}`** / **`ToolResult{name, ok, summary}`**（后两个仅 Agent 模式产生）

**ServerEvent**（`server.rs`）：`{ event: String, payload: Value }`，
经 SSE 下发后由 shim 派发为 `listen(event, cb)` 回调。

---

## 六、协议实现细节（三种 ptype + Agent 模式）

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
2. **chat-requirements**：POST `/backend-api/sentinel/chat-requirements`，body `{"p": ""}`
   → 返回 `{token, proofofwork: {required, seed, difficulty}}`
   > **`p` 必须传空串**。这个字段原本是「首包 PoW 证明」（`gAAAAAC` + 长 base64），
   > 但 2026-10 起服务端对长 `p` 一律回 `HTTP 500 {"detail":"Internal Server Error"}`，
   > 空 `p` 反而正常返回凭证。真正需要的 PoW 走下一步的 `proofofwork` 挑战。
3. **PoW 求解**（`solve_pow`）：
   - 构造 **18 元素** JSON 数组并 base64 编码，前缀 `gAAAAAB` 即 proof token；
   - 计算 `SHA3-512(seed_bytes ‖ base64_bytes)`；
   - 要求摘要前 N 字节 ≤ `difficulty`（十六进制解码后逐字节比较）；
   - 命中即返回该 base64 串作为 proof。
   - **数组排布（踩过两次坑，改错就 500 或 403）**：
     `[3000, time, 4294705152, i, i>>1, UA, script, dpl, "en-US", "en-US,es-US,en,es",
       nav, "__NEXT_DATA__", "__next_set_public_path__", perf, uuid, "", 8, timediff]`
     - `i` 在 **index 3**，`i>>1` 紧跟其后的 **index 4**（实测放 index 9 会 403）；
     - 整段 JSON **必须以 `]` 结尾**，漏了就是 `HTTP 500 "Hmm...something seems to have gone wrong."`；
     - 回归测试 `pow_payload_is_valid_json` 守这两点。
   - 实现细节：零分配内循环 + 手写 u64 → ASCII 转换（`push_u64`），
     **release 下 `000032` 难度约 1 秒**；debug 下极慢（正常，勿用 debug 测）。
   - **一个 token 只能用一次**：chat-requirements 的 `token` 与 proof 是一次性的，
     复用会 403 `Unusual activity has been detected`。每次对话都要重新申请。
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

**OAuth 登录（一键，全自动）**：账号通过官方 OAuth（PKCE 授权码）获得，
**不需要复制网址、不需要书签**——点一下按钮，浏览器里登录完就自动回来了。

实现对齐 OpenAI 官方开源的 Codex CLI（`codex-rs/login/src/server.rs`）：
Auth0 给 Codex 这个公开客户端注册了 **loopback 回调**（任意 localhost 端口合法），
所以把回调直接指向本服务自己：

1. `POST /api/oauth/start` 生成 `code_verifier`/`state`，
   `redirect_uri = http://127.0.0.1:<本服务端口>/auth/callback`；
2. 前端 `window.open(authorize_url)`，用户在浏览器里正常登录 OpenAI；
3. OpenAI 把浏览器 302 回**本机服务**的 `/auth/callback?code=...&state=...`，
   服务端直接拿 code 换 token（form-urlencoded POST `auth.openai.com/oauth/token`）；
4. 落账号池（JWT 解析 email / `chatgpt_account_id` / `plan_type`），
   浏览器那页显示「登录成功，本页会自动关闭」，前端收 `login-captured` 事件自动收尾；
5. 之后每次对话前检查 AT 临期（<5 分钟）即用 refresh_token 自动续期并写回账号池。

授权参数：`client_id = app_EMoamEEZ73f0CkXaXp7hrann`（Codex 公开客户端，无 secret），
`scope = openid profile email offline_access api.connectors.read api.connectors.invoke`，
附加 `codex_cli_simplified_flow=true&id_token_add_organizations=true&originator=codex_cli_rs`。

> **PKCE 算法必须是 SHA-256**（`code_challenge_method=S256` 的 S256 指 SHA-256，
> 不是 SHA3-256）。用错算法时授权页照样能过，但换 token 会被 OpenAI 拒绝并返回
> `token_exchange_user_error`，极难排查。`oauth::tests::pkce_challenge_matches_rfc7636`
> 用 RFC 7636 附录 B 的测试向量守住这一点。
>
> 若 loopback 因防火墙/代理被拦，弹窗里还有「自动登录失败？手动完成」折叠区：
> 把回调页地址栏 URL 粘进去点「手动完成登录」，走 `POST /api/oauth/finish` 兜底。

### 6.3.1 `openai` —— 通用 OpenAI 兼容协议（主力通道）

这是最通用的一条链路，DeepSeek / 各类中转 / 自建 vLLM / OneAPI 都走它。
适配细节（都踩过坑）：

| 问题 | 处理 |
| --- | --- |
| `base_url` 写 `https://x`、`https://x/v1`、`https://x/v1/` 都有人写 | `providers::chat_urls()` 生成候选列表，遇 404/405 自动换下一个候选 |
| 部分网关用 CRLF 分帧 SSE | 收到 chunk 先 `replace("
", "
")` 再按空行切 |
| 推理模型把思考过程放在 `delta.reasoning_content` | 单独解析成 `ChatEvent::Reasoning` → SSE `chat_reasoning` → 前端折叠块「思考过程」 |
| 有的端点先回 200，再在流里塞 `{"error":...}` | 解析时识别并直接报错，不会静默变成空回复 |
| 错误信息只有状态码，无法排查 | `classify_error` 提取 `error.message` 原文，按 400/401/402/403/404/422/429 分别给中文提示 |
| 不知道模型名该填什么 | 设置页每个端点有「拉取模型」按钮 → `POST /api/models` → `GET {base}/models`（同样带 /v1 回退），自动填进模型列表 |
| 推理模型不接受 `temperature` | 请求体不再硬塞 `temperature`，交给服务端默认值 |

### 6.3.2 `chatgpt_web` —— ChatGPT 网页版（含 OAuth 账号）

请求头是关键。OAuth（Codex 客户端）签发的 access_token 带组织信息，
backend-api 要求**显式声明用哪个账号**，否则 sentinel 接口会抛
`HTTP 500 {"detail":"Internal Server Error"}`（FastAPI 里取 header 失败）。

必备头：

| 头 | 值 | 说明 |
| --- | --- | --- |
| `Authorization` | `Bearer <access_token>` | |
| `chatgpt-account-id` | 账号 meta 里的 `chatgpt_account_id` | **缺了就是 500**；没有 meta 时从 token 的 `https://api.openai.com/auth` claim 里读 |
| `oai-device-id` | 由账号 ID 派生的稳定 UUID | 每次随机会被当成换设备 |
| `oai-language` / `Origin` / `Referer` | `en-US` / `https://chatgpt.com` / `https://chatgpt.com/` | |
| `sec-ch-ua` / `sec-fetch-*` | 见 `chatgpt_headers()` | 伪装成浏览器 |

**兜底**：sentinel 返回 500/401/403 时，自动改走 Codex 官方接口
`POST /backend-api/codex/responses`（Responses API 格式，SSE 事件名
`response.output_text.delta`）。这才是 OAuth token 的原生用法，
如果当前模型名不被接受（网页版模型和 Codex 模型不是一套），会自动换 `gpt-5` 再试一次。

### 6.4 Agent 模式 —— 模型自主调用工具（仅 API 端点）

输入框旁的 🧩 按钮切换。开启后 `POST /api/chat` 带 `agent_mode: true`，
`chat::stream_chat` 把请求分流到 `agent.rs`，由**模型自己决定**是否调用工具、调几次。

**红线（设计决策，勿改）**：

> Agent **只走 API 通道**（`openai` 兼容 + `gemini_native`）。
> `chatgpt_web` 强制不走 Agent——网页端逆向接口 + 高频工具循环几乎必然触发风控封号。
> 对 `chatgpt_web` 开启 Agent 时，前端会 toast 提示「不支持 Agent（易封号）」，后端同样有兜底分支。

**四个内置工具**（`agent_tools.rs`，无外部依赖、免 key）：

| 工具 | 参数 | 实现要点 |
|---|---|---|
| `get_current_time` | 无 | `chrono::Local` + `Datelike`，输出中文格式 |
| `calculate` | `expr` | 手写 tokenize → 逆波兰（RPN）→ 栈求值。**不用 eval**，防注入 |
| `web_search` | `query` | DuckDuckGo HTML 端点（免 key）优先，失败回退 Bing；解析标题 + URL + 摘要 |
| `fetch_url` | `url` | GET 后剥 `<script>`/`<style>` 取正文，上限 12000 字符 |

**OpenAI 兼容协议循环**（`run_openai_agent`）：

1. 请求体带 `tools: [...]`（`openai_tools(allow_web)` 生成，关联网时可去掉 `web_search`/`fetch_url`）；
2. 流式响应按 `choices[0].delta.tool_calls[].index` **拼装**分片的工具调用（name/arguments 都是增量到达）；
3. 收到完整 tool_calls → 推 `ChatEvent::ToolCall` → `run_tool()` 执行 → 结果以
   `{"role":"tool","tool_call_id":..., "content": ...}` 消息**回填**再请求下一轮；
4. 模型回纯文本 → 走普通 `Delta` 推送，结束。上限 `max_steps = 6` 轮，防无限循环；
5. 账号池 failover 与 `chat_urls()` 404/405 回退和普通链路完全一致。

**Gemini 原生协议循环**（`run_gemini_agent`）：

1. 请求体带 `tools: [{functionDeclarations: [...]}]`（`gemini_tools(allow_web)`）；
2. `streamGenerateContent?alt=sse&key=` 流式解析 `/candidates/0/content/parts`；
3. 出现 `functionCall` part → 推 `ToolCall` → 执行 → 以
   `{"role":"user","parts":[{"functionResponse":{"name","response":{...}}}]}` 回填；
4. 同样 `max_steps = 6` 封顶。

**前端呈现**：SSE 事件 `agent_tool`（`phase: "call" | "result"`）驱动时间线组件——
每个工具调用一张卡片，运行中转圈、成功 ✓、失败 ✕，附参数与结果摘要（`brief()` 截断）。
模型最终答复照常走打字机渲染，与 ChatGPT 的「思维链/工具」体验对齐。

**真机验证（2026-10-24）**：

- Gemini 单轮双工具：`get_current_time` + `calculate`，答「现在是2026年10月24日星期六10:16:59。(128+37)*6 的计算结果是 990。」
- Gemini 联网搜索：自主 `web_search("Rust programming language latest stable version")`
  → `fetch_url("https://releases.rs/")` → 答「Rust 编程语言最新的稳定版本是 1.99.1。」
  验证脚本：`verify-agent.cjs`（工具调用）、`verify-agent-search.cjs`（联网搜索）。

> ⚠️ 网页协议（`chatgpt_web`）的接口细节来自开源项目 `lanqian528/chat2api` 与 `caiwuu/web2api` 的源码分析。
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
| POST | `/api/verify-account` | **添加前真实校验**：Gemini 走 `models`，ChatGPT 走 session 接口，其余（OpenAI 兼容，含 Claude API）走 `/models` 探针 |
| POST | `/api/models` | 拉取可用模型列表 |
| GET | `/api/version` | 版本 + 构建时间（界面底部展示，用于核对 exe 新旧） |
| POST | `/api/chat` | 发起对话（body: provider_id / model / messages / conv_id / image_mode / **agent_mode**） |
| POST | `/api/chat/stop` | 停止某会话的流式输出（body: `{conv_id}`） |
| POST | `/api/upload` | 原始字节上传附件（query: name/mime，body 为字节） |
| GET | `/api/files/:id` | 读取上传的附件文件 |
| GET | `/api/images/:id` | 读取生图结果（png） |
| POST | `/api/oauth/start` | ChatGPT OAuth 第一步：生成 PKCE + loopback authorize URL |
| GET | `/auth/callback` | **OAuth 自动回调**：浏览器被 302 回来，服务端直接换 token 落账号池 |
| POST | `/api/oauth/finish` | OAuth 手动兜底：粘贴回调 URL 换 token |
| GET | `/api/conversations` | 会话列表 |
| POST | `/api/conversations` | 新建会话 |
| GET | `/api/conversations/:id` | 读取会话（含 messages、meta） |
| DELETE | `/api/conversations/:id` | 删除会话 |
| POST | `/api/conversations/:id/rename` | 重命名 |
| POST | `/api/conversations/:id/messages` | 追加一条消息（body 支持 `attachments`） |
| GET | `/api/events` | **SSE 长连接**，推送 `chat_stream` / `account_ok` / `account_switch` / `login-captured` / **`agent_tool`** 等 |
| POST | `/api/quit` | 页面关闭通知（sendBeacon）：设 8s 退出死线，期间任何请求都会取消 |
| POST | `/api/ping` | 前端 5s 心跳，用于「还有标签页开着」的存活判定 |
| GET | `/api/debug-exit` | 诊断：查看退出死线 / SSE 计数 / should_quit |
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
    "ptype": "openai",              // openai | gemini_native | chatgpt_web
    "base_url": "https://api.deepseek.com/v1",
    "models": ["deepseek-chat"],
    "default_model": "deepseek-chat",
    "proxy": null,
    "enabled": true,
    "accounts": [{
      "id": "…",
      "name": "我的账号",
      "cred_type": "api_key",       // api_key | token
      "secret": "sk-…",             // API Key / accessToken
      "cookie": null,               // ChatGPT 网页版用
      "meta": null,                 // ChatGPT 的 chatgpt_account_id 等
      "status": "active",           // active | disabled | error | expired
      "priority": 0
    }]
  }]
}
```

> Claude 已改为普通 `openai` 类型 provider（`base_url = https://api.anthropic.com/v1`，
> 默认 `enabled: false`）。删掉的 `claude_web` 类型由 `config::backfill_chatgpt_meta()` 等
> 启动迁移逻辑保证旧配置不炸。

### 会话 meta（网页协议多轮状态）

```json
{
  "conversation_id": "…",       // ChatGPT
  "parent_message_id": "…",     // ChatGPT
  "chatgpt_account_id": "…",    // ChatGPT（启动时自动 backfill）
  "plan_type": "free"           // ChatGPT
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

| 变量 | 作用 |
|---|---|
| `AI_HUB_NO_OPEN=1` | 阻止自动打开浏览器（CI / 调试用） |
| `AI_HUB_PORT=9000` | 指定端口（等价 `--port`） |
| `AI_HUB_DEBUG_POW=1` | 打开网页版对话失败时的详细日志：把 seed/proof 写到 `%TEMP%\ai-hub-proof.txt`，并把服务端原始响应打到 stderr |

---

## 十、测试

### 已建立的测试手段

| 类型 | 做法 |
|---|---|
| 单元测试 | `cargo test`（21 项）：PKCE RFC 向量、JWT claim 解析、PoW 求解与**载荷 JSON 合法性**、设备 ID 稳定性、base_url 归一化、RPN 计算器、HTML 剥标签、PDF 字符串/八进制转义、Office 魔数识别 |
| 协议级 mock | Python 起 mock 服务，严格复刻 ChatGPT sentinel+PoW、Gemini 原生 SSE |
| API 级 | Python/Node 脚本走 REST + SSE，校验逐字增量与事件序列 |
| **真机在线验证** | `verify-live.cjs <provider>`、`verify-chatgpt-live.cjs`、`verify-chatgpt-multi.cjs`、`verify-agent.cjs`、`verify-agent-search.cjs` |
| UI 级 | Playwright 真实 Chromium，检查 DOM、弹窗、零 JS 错误、截图 |

### 关键验证结论（2026-10-24 更新）

- `cargo test` **21/21 通过**
- **ChatGPT 真机**：单轮 `只回复两个字：收到` ✅；多轮记忆（7391）✅
- **Gemini 真机**：`gemini-2.5-flash` ✅
- **Agent 真机（Gemini）**：双工具调用 ✅；联网搜索 → 抓网页 → 作答闭环 ✅
- **Agent（OpenAI 兼容）**：代码路径已实现，无 Key 未实测（见「待办」）
- 通用 OpenAI 兼容：6 项端到端全过（404 回退 / 思考过程 / 可读错误 / 拉取模型）
- 退出逻辑：无页面 9s 退、页面开着不误杀、页面离开 11.5s 退

### 复现测试的建议顺序

1. `cargo test` —— 先确认 PoW、RPN、文档提取没坏
2. 起服务后 `node verify-live.cjs chatgpt` —— 真机冒烟
3. 起服务后 `node verify-agent.cjs gemini` —— Agent 工具调用真机冒烟
4. 起 mock → 改 `base_url` 指向 mock → 走完整 chat（`verify-openai.cjs`）
5. Playwright 跑 UI 回归（注意用 `domcontentloaded`，**不要用 `networkidle`**，
   因为 SSE 长连接会让网络永不空闲）

---

## 十一、已知问题（重要，按优先级）

### 1. ~~缺少「接入方式」切换 UI~~ —— 已修复

每张服务卡现有「接入方式」下拉：ChatGPT 可切「网页版 ⇄ OpenAI 兼容（反代）」，
Gemini 可切「官方原生 ⇄ OpenAI 兼容层」，切换时联动字段标签与账号池按钮。

### 2. ~~账号池两个按钮对网页版端点行为相同~~ —— 已修复

`renderAccounts()` 现按 `ptype` 只显示一个按钮：网页版端点只显示「网页登录抓 Token」，
Key 类端点只显示「＋ 手动填 Key」。

### 3. ~~网页版协议无真实账号验证~~ —— 已解决（2026-10-24）

ChatGPT 用真实 OAuth 账号跑通了单轮与多轮。过程中修掉三个真机才暴露的问题：
`chatgpt-account-id` 缺失导致 sentinel 500、JWT claim key 含 `/` 解析不出账号 ID、
PoW 载荷 JSON 缺结尾 `]` 且 `i>>1` 位置错（详见 6.3 与 6.3.2）。

真机再失效时的排查顺序：
`chatgpt-account-id` → sentinel 返回体 → PoW 载荷是否合法 JSON → conversation 请求体字段。

### 3.5 ~~Claude 网页版~~ —— 已删除（2026-10-24）

协议层曾跑通（400 / model_not_available 都修掉了），但测试账号稳定 429 限流，
且网页逆向 + Agent 工具循环叠加后封号风险不可接受。按用户决策整体删除
（约 300 行：`claude_send` / `claude_headers` / `verify_claude` / `handle_claude_event` /
`claude_web` 分发分支 / 前端抓取弹窗）。现 Claude 以官方 API 形态保留为
`openai` 类型 provider，默认关闭，需要时在设置里打开即可。

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

### 9. Agent 的已知局限

- **OpenAI 兼容协议未真机验证**：`run_openai_agent` 的 tool_calls 拼装 / `role:"tool"` 回填
  只有代码与 mock 级保障，等有 Key 后跑 `verify-agent.cjs openai` 补验。
- **无并发工具调用压力测试**：模型一次回多个 tool_calls 时按序执行（未并行），
  多个 `web_search` 连发时总耗时 = 各次之和。
- **Agent 与网页版互斥**：`chatgpt_web` 下 Agent 开关点了也会被后端忽略（防封号），
  前端会 toast 说明。
- **工具结果截断**：`fetch_url` 上限 12000 字符、`web_search` 上限 8 条，
  超长内容由 `brief()` 再截断，模型可能拿不到完整信息。
- **上传文本注入**：上传的 txt/md/csv 会全文内联进 prompt，无长度上限保护
  （当前靠 `attachments` 体积直觉约束），极端大文件建议后续加截断。

---

## 十二、后续待办建议

按性价比排序：

1. **Agent 的 OpenAI 兼容协议真机回归** —— 有任意 OpenAI 兼容 Key（DeepSeek / 中转）即可跑
   `verify-agent.cjs openai`，补上 `role:"tool"` 回填的真机验证
2. **OAuth / 网页版真实账号回归测试** —— ChatGPT OAuth 登录续期已跑通一次，定期回归记录失效点
3. **Gemini 原生生图**（imagen / 图片预览模型走 `generateContent`）
4. **ChatGPT 网页版生图**（`/backend-api/files` 上传 + 多模态消息）
5. **Agent 体验深化**：工具调用并行化、`web_search` 结果缓存、上传文档直接喂 Agent 总结
6. **安装包**：NSIS / MSI，带开始菜单快捷方式与卸载
7. **托盘 + 单实例**：避免重复启动起多个服务（固定端口已部分缓解）
8. **会话搜索 / 导出 Markdown**
9. **代理配置细化**：目前是整服务一个 proxy，可细化到「仅网页版走代理」

---

## 十三、合规与风险声明

- 本项目是**可配置端点的客户端**：不内置、不分发任何 API Key 或共享账号。
- `chatgpt_web` 通过**用户自己的登录态**访问网页接口，
  这类用法**可能违反对应服务的条款**，存在封号风险，由使用者自行承担。
- 正因如此，**Agent（工具循环）被限制为只走 API 通道**，不碰网页端——这是产品决策而非技术限制。
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
