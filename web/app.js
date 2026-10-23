/* AI Hub 前端：纯原生 JS（零框架），通过 window.__TAURI__ 调用 Rust 命令 */
(function () {
  "use strict";
  const TAURI = window.__TAURI__;
  const invoke = TAURI.core.invoke;
  const listen = TAURI.event.listen;
  const WebviewWindow = TAURI.window.WebviewWindow;

  const DEFAULT_SELECTOR =
    "(function(){try{var s=localStorage.getItem('token')||localStorage.getItem('apiKey')||localStorage.getItem('access_token')||localStorage.getItem('chatgpt_token')||localStorage.getItem('session');if(s&&s.length>8)return s;var els=document.querySelectorAll('#api-key,#token,#apiKey,input[name=token],textarea.token,pre.token,code.token');for(var i=0;i<els.length;i++){var v=els[i].value||els[i].textContent;if(v&&v.trim().length>8)return v.trim();}return '';}catch(e){return '';}})()";

  // ---------- 状态 ----------
  let cfg = null;
  let convs = [];
  let curConv = null;
  let curMessages = [];
  let curProviderId = null;
  let curModel = null;
  let streaming = false;
  let capturePid = null; // 当前正在抓取登录的 provider id
  let keyPid = null; // 当前正在添加 Key 的 provider id

  // ---------- 工具 ----------
  const $ = (id) => document.getElementById(id);
  function el(html) {
    const t = document.createElement("template");
    t.innerHTML = html.trim();
    return t.content.firstElementChild;
  }
  function escapeHtml(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
    );
  }
  function sanitizeHtml(html) {
    return html
      .replace(/<script[\s\S]*?<\/script>/gi, "")
      .replace(/<iframe[\s\S]*?<\/iframe>/gi, "")
      .replace(/ on\w+="[^"]*"/gi, "")
      .replace(/ on\w+='[^']*'/gi, "")
      .replace(/javascript:/gi, "");
  }
  function renderMd(text) {
    try {
      return sanitizeHtml(window.marked.parse(text || ""));
    } catch (e) {
      return escapeHtml(text || "");
    }
  }
  const curProvider = () => cfg.providers.find((p) => p.id === curProviderId);
  function toast(msg, type) {
    const t = el(`<div class="toast ${type || ""}">${escapeHtml(msg)}</div>`);
    $("toasts").appendChild(t);
    setTimeout(() => {
      t.style.opacity = "0";
      setTimeout(() => t.remove(), 300);
    }, 4200);
  }
  function scrollBottom() {
    const b = $("messages");
    b.scrollTop = b.scrollHeight;
  }

  // ---------- 初始化 ----------
  async function init() {
    await loadConfig();
    setupListeners();
    await loadConvs();
    renderProviders();
    renderModels();
    renderAccountChip();
    bindUI();
    loadBuildInfo();
    if (!curConv) await newChat();
  }

  /** 显示构建信息，方便核对 exe 是否最新（Tauri 版无此接口，静默跳过）。 */
  async function loadBuildInfo() {
    try {
      const v = await fetch("/api/version").then((r) => r.json());
      const box = $("buildInfo");
      if (!box) return;
      const d = new Date((v.build_ts || 0) * 1000);
      const p2 = (n) => String(n).padStart(2, "0");
      box.textContent =
        `v${v.version} · ${v.mode} · 构建 ` +
        `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())} ` +
        `${p2(d.getHours())}:${p2(d.getMinutes())}`;
    } catch (e) {
      /* 非浏览器版（Tauri）没有该接口，忽略 */
    }
  }

  async function loadConfig() {
    cfg = await invoke("get-config");
    curProviderId = cfg.selected_provider || (cfg.providers[0] && cfg.providers[0].id);
    const p = curProvider();
    curModel = p ? p.default_model : "";
  }
  async function loadConvs() {
    convs = await invoke("list-conversations");
    renderConvs();
  }

  function setupListeners() {
    listen("chat_stream", (e) => {
      const d = e.payload;
      if (d.conv_id !== curConv) return;
      const live = document.querySelector('.msg.assistant[data-live="1"]');
      if (!live) return;
      const node = live.querySelector(".body");
      if (d.done) {
        const full = node.dataset.full || "";
        const errHtml = d.error ? `<p class="err">${escapeHtml(d.error)}</p>` : "";
        live.removeAttribute("data-live");
        node.innerHTML = renderMd(full) + errHtml;
        saveMsg("assistant", full);
        streaming = false;
        $("send").classList.remove("hidden");
        return;
      }
      if (d.error) {
        node.innerHTML += `<p class="err">${escapeHtml(d.error)}</p>`;
        return;
      }
      const full = (node.dataset.full || "") + d.delta;
      node.dataset.full = full;
      node.innerHTML = renderMd(full) + '<span class="caret">▍</span>';
      scrollBottom();
    });
    listen("account_switch", (e) => {
      const d = e.payload;
      toast(`账号「${d.account}」异常：${d.reason}，已自动切换下一个`, "warn");
      renderAccountChip();
    });
    listen("account_ok", (e) => {
      setChip(e.payload.account, "ok");
    });
  }

  // ---------- 渲染：服务商 / 模型 / 账号 ----------
  function renderProviders() {
    const sel = $("providerSel");
    sel.innerHTML = "";
    cfg.providers
      .filter((p) => p.enabled)
      .forEach((p) => {
        const o = el(`<option value="${p.id}">${escapeHtml(p.name)}</option>`);
        if (p.id === curProviderId) o.selected = true;
        sel.appendChild(o);
      });
  }
  function renderModels() {
    const p = curProvider();
    const sel = $("modelSel");
    sel.innerHTML = "";
    if (!p) return;
    p.models.forEach((m) => {
      const o = el(`<option value="${escapeHtml(m)}">${escapeHtml(m)}</option>`);
      if (m === curModel) o.selected = true;
      sel.appendChild(o);
    });
  }
  function renderAccountChip() {
    const p = curProvider();
    const chip = $("accountChip");
    if (!p) return;
    const total = p.accounts.filter((a) => a.status !== "disabled").length;
    if (p.preferred_account) {
      const a = p.accounts.find((x) => x.id === p.preferred_account);
      if (a) {
        chip.className = "account-chip " + (a.status === "error" ? "bad" : "");
        chip.querySelector(".txt").textContent = `指定：${a.name}`;
        return;
      }
    }
    chip.className = "account-chip";
    chip.querySelector(".txt").textContent = `自动轮换（${total} 个账号）`;
  }
  function setChip(name, kind) {
    const chip = $("accountChip");
    chip.className = "account-chip" + (kind === "ok" ? "" : " warn");
    chip.querySelector(".txt").textContent = `当前账号：${name}`;
  }

  // ---------- 会话 ----------
  function renderConvs() {
    const list = $("convList");
    list.innerHTML = "";
    convs.forEach((c) => {
      const item = el(
        `<div class="conv-item ${c.id === curConv ? "active" : ""}" data-id="${c.id}">
          <div class="conv-title">${escapeHtml(c.title || "新对话")}</div>
          <div class="conv-del" title="删除">🗑</div>
        </div>`
      );
      item.querySelector(".conv-title").onclick = () => selectConv(c.id);
      item.querySelector(".conv-del").onclick = (ev) => {
        ev.stopPropagation();
        deleteConv(c.id);
      };
      item.querySelector(".conv-title").ondblclick = () => renameConv(c.id);
      list.appendChild(item);
    });
  }
  async function newChat() {
    const id = "c-" + Date.now() + "-" + Math.floor(Math.random() * 1e4);
    const c = await invoke("create-conversation", {
      id,
      title: "新对话",
      provider: curProviderId,
      model: curModel,
    });
    convs.unshift(c);
    curConv = id;
    curMessages = [];
    renderConvs();
    $("messages").innerHTML = "";
  }
  async function selectConv(id) {
    curConv = id;
    const c = await invoke("get-conversation", { id });
    curMessages = (c.messages || []).map((m) => ({ role: m.role, content: m.content }));
    renderConvs();
    const box = $("messages");
    box.innerHTML = "";
    curMessages.forEach((m) => appendMsgEl(m.role, m.content, false));
    scrollBottom();
  }
  async function deleteConv(id) {
    if (!confirm("确定删除该会话？")) return;
    await invoke("delete-conversation", { id });
    convs = convs.filter((c) => c.id !== id);
    if (curConv === id) {
      curConv = null;
      curMessages = [];
      $("messages").innerHTML = "";
    }
    renderConvs();
    if (!curConv) await newChat();
  }
  async function renameConv(id) {
    const c = convs.find((x) => x.id === id);
    const t = prompt("重命名会话", c ? c.title : "");
    if (!t) return;
    await invoke("rename-conversation", { id, title: t });
    await loadConvs();
  }
  function appendMsgEl(role, content, live) {
    const box = $("messages");
    const m = el(
      `<div class="msg ${role} ${live ? "live" : ""}" ${live ? 'data-live="1"' : ""}>
        <div class="av">${role === "user" ? "我" : "AI"}</div>
        <div class="body">${renderMd(content)}</div>
      </div>`
    );
    box.appendChild(m);
    scrollBottom();
    return m;
  }
  function saveMsg(role, content) {
    if (!curConv) return;
    invoke("save-message", { id: curConv, role, content }).catch(() => {});
  }

  // ---------- 发送 ----------
  async function send() {
    if (streaming) return;
    const input = $("input");
    const text = input.value.trim();
    if (!text) return;
    if (!curConv) await newChat();

    appendMsgEl("user", text, false);
    curMessages.push({ role: "user", content: text });
    saveMsg("user", text);
    input.value = "";

    const live = appendMsgEl("assistant", "", true);
    live.querySelector(".body").dataset.full = "";
    streaming = true;
    $("send").classList.add("hidden");

    const p = curProvider();
    try {
      await invoke("chat-stream", {
        provider_id: curProviderId,
        model: curModel,
        messages: curMessages.map((m) => ({ role: m.role, content: m.content })),
        conv_id: curConv,
      });
    } catch (e) {
      const node = document.querySelector('.msg.assistant[data-live="1"] .body');
      if (node) node.innerHTML += `<p class="err">${escapeHtml(e)}</p>`;
      streaming = false;
      $("send").classList.remove("hidden");
      toast("请求失败：" + e, "err");
    }
  }

  // ---------- 设置 / 账号池 ----------
  function openSettings() {
    $("settingsModal").classList.remove("hidden");
    renderProviderCards();
    renderAcctProviderSel();
    renderAccounts($("acctProviderSel").value);
  }
  function closeSettings() {
    $("settingsModal").classList.add("hidden");
  }
  function protoLabel(ptype) {
    return (
      {
        chatgpt_web: "ChatGPT 网页版（PoW）",
        claude_web: "Claude 网页版（sessionKey）",
        gemini_native: "Gemini 原生协议",
        openai: "OpenAI 兼容接口",
      }[ptype] || ptype
    );
  }

  function renderProviderCards() {
    const box = $("providerCards");
    box.innerHTML = "";
    cfg.providers.forEach((p) => {
      const isWeb = p.ptype === "chatgpt_web" || p.ptype === "claude_web";
      const isGemini = p.ptype === "gemini_native";
      const urlLabel = isWeb
        ? "站点地址（协议根地址）"
        : isGemini
        ? "基础 URL（Gemini 原生 /v1beta）"
        : "基础 URL（OpenAI 兼容 /v1）";
      const loginField = isWeb
        ? `<div class="field"><label>站点首页</label>
             <input data-f="login_url" value="${escapeHtml(p.login_url || "")}"></div>`
        : "";
      const selectorField =
        isWeb || isGemini
          ? ""
          : `<div class="field"><label>抓取 Token 的 JS 表达式（反代可忽略）</label>
             <textarea data-f="login_token_selector" rows="2">${escapeHtml(p.login_token_selector || DEFAULT_SELECTOR)}</textarea></div>`;
      const card = el(
        `<div class="prov-card" data-pid="${p.id}">
          <h4>${escapeHtml(p.name)} ${p.enabled ? "" : "（已停用）"}
            <span class="pill">${escapeHtml(protoLabel(p.ptype))}</span></h4>
          <div class="row">
            <div class="field"><label>${urlLabel}</label>
              <input data-f="base_url" value="${escapeHtml(p.base_url)}"></div>
            ${loginField}
          </div>
          <div class="row">
            <div class="field"><label>默认模型</label>
              <input data-f="default_model" value="${escapeHtml(p.default_model)}"></div>
            <div class="field"><label>代理 proxy（可选）</label>
              <input data-f="proxy" value="${escapeHtml(p.proxy || "")}" placeholder="http://127.0.0.1:7890"></div>
          </div>
          <div class="field"><label>模型列表（逗号分隔）</label>
            <input data-f="models" value="${escapeHtml(p.models.join(", "))}"></div>
          ${selectorField}
          <div class="prov-actions">
            <label class="chk"><input type="checkbox" data-f="enabled" ${p.enabled ? "checked" : ""}> 启用</label>
            ${p.id === "gemini" || p.id === "chatgpt" || p.id === "claude" || p.id === "deepseek" ? "" : '<button class="ghost sm del-prov">删除该端点</button>'}
          </div>
        </div>`
      );
      card.querySelectorAll("input,textarea").forEach((inp) => {
        inp.addEventListener("input", () => {
          const f = inp.dataset.f;
          const prov = cfg.providers.find((x) => x.id === p.id);
          if (f === "enabled") prov.enabled = inp.checked;
          else if (f === "models") prov.models = inp.value.split(",").map((s) => s.trim()).filter(Boolean);
          else prov[f] = inp.value;
        });
      });
      const del = card.querySelector(".del-prov");
      if (del)
        del.onclick = () => {
          if (!confirm("删除该自定义端点？")) return;
          cfg.providers = cfg.providers.filter((x) => x.id !== p.id);
          renderProviderCards();
        };
      box.appendChild(card);
    });
  }
  function renderAcctProviderSel() {
    const sel = $("acctProviderSel");
    sel.innerHTML = "";
    cfg.providers.forEach((p) => {
      const o = el(`<option value="${p.id}">${escapeHtml(p.name)}</option>`);
      sel.appendChild(o);
    });
  }
  function renderAccounts(pid) {
    const p = cfg.providers.find((x) => x.id === pid);
    const box = $("accountList");
    box.innerHTML = "";
    if (!p) return;

    // 按钮文案跟随端点类型：Key 类显示「添加 API Key」，网页版显示「网页登录抓 Token」
    const isWebProvider = p.ptype === "chatgpt_web" || p.ptype === "claude_web";
    const loginBtn = $("webLoginBtn");
    loginBtn.textContent = isWebProvider ? "🌐 网页登录抓 Token" : "＋ 添加 API Key";
    $("keyAddBtn").classList.toggle("hidden", !isWebProvider);

    const list = [...p.accounts].sort((a, b) => a.priority - b.priority);
    if (list.length === 0) {
      const isWeb = p.ptype === "chatgpt_web" || p.ptype === "claude_web";
      const empty = el(`<div class="acc-empty">
        <div class="hint">${
          isWeb
            ? "该服务还没有账号。点「🌐 网页登录抓 Token」按引导完成登录。"
            : "该服务还没有账号。点下面按钮，粘贴 API Key 即可（会自动验证）。"
        }</div>
        <div><button class="primary sm acc-empty-add">＋ 添加第一个账号</button></div>
      </div>`);
      empty.querySelector(".acc-empty-add").onclick = () =>
        isWeb ? openCapture(p.id) : openKeyModal(p.id);
      box.appendChild(empty);
      return;
    }
    list.forEach((a) => {
      const isPref = p.preferred_account === a.id;
      const row = el(
        `<div class="acc-row" data-aid="${a.id}">
          <div class="acc-main">
            <div class="acc-name">${escapeHtml(a.name)}
              <span class="pill ${a.status}">${statusText(a.status)}</span>
              ${isPref ? '<span class="pill pref">首选</span>' : ""}
              <span class="pill">${a.cred_type === "token" ? "网页Token" : "API Key"}</span>
            </div>
            <div class="acc-meta">${escapeHtml(a.last_error || (a.last_used ? "上次使用：" + a.last_used : "未使用"))}</div>
          </div>
          <div class="acc-actions">
            <button class="ghost sm act-pref">${isPref ? "取消首选" : "指定首选"}</button>
            <button class="ghost sm act-toggle">${a.status === "disabled" ? "启用" : "停用"}</button>
            <button class="ghost sm act-up">↑</button>
            <button class="ghost sm act-down">↓</button>
            <button class="ghost sm act-del">删除</button>
          </div>
        </div>`
      );
      row.querySelector(".act-pref").onclick = () => togglePreferred(pid, a.id);
      row.querySelector(".act-toggle").onclick = () => toggleDisabled(pid, a.id, a.status);
      row.querySelector(".act-up").onclick = () => movePriority(pid, a.id, -1);
      row.querySelector(".act-down").onclick = () => movePriority(pid, a.id, 1);
      row.querySelector(".act-del").onclick = () => delAccount(pid, a.id);
      box.appendChild(row);
    });
  }
  function statusText(s) {
    return { active: "正常", disabled: "停用", error: "异常", expired: "过期" }[s] || s;
  }

  // ---------- 添加 API Key（正式表单，替代会被浏览器屏蔽的 prompt） ----------
  const KEY_GUIDE = {
    gemini: {
      title: "添加 Gemini API Key",
      html:
        "<b>Gemini 原生协议 · 不需要任何反代</b><br>" +
        "1. 打开 Google AI Studio，点 <code>Create API key</code><br>" +
        "2. 复制以 <code>AIza</code> 开头的字符串<br>" +
        "3. 粘贴到下面 → 点「验证并添加」，通过后即可直接聊天",
      link: "https://aistudio.google.com/apikey",
      linkText: "打开 AI Studio 获取 Key",
      ph: "AIza...",
    },
    deepseek: {
      title: "添加 DeepSeek API Key",
      html:
        "<b>DeepSeek 官方 API</b><br>" +
        "1. 打开 DeepSeek 开放平台 → API keys → 创建<br>" +
        "2. 复制以 <code>sk-</code> 开头的 Key<br>" +
        "3. 粘贴到下面 → 验证并添加",
      link: "https://platform.deepseek.com/api_keys",
      linkText: "打开 DeepSeek 开放平台",
      ph: "sk-...",
    },
  };

  function guideFor(p) {
    if (KEY_GUIDE[p.id]) return KEY_GUIDE[p.id];
    if (p.ptype === "gemini_native") return KEY_GUIDE.gemini;
    return {
      title: `添加 ${p.name} 的 API Key`,
      html:
        "<b>自定义 / 反代端点</b><br>" +
        "请先在「服务」页把 Base URL 填好（形如 <code>https://host/v1</code>），" +
        "再把该端点签发的 Key 粘贴到下面。",
      link: "",
      linkText: "",
      ph: "sk-...",
    };
  }

  function setKeyStatus(html, cls) {
    const box = $("keyStatus");
    box.className = "cap-status " + (cls || "");
    box.innerHTML = html;
  }

  function openKeyModal(pid, keep) {
    const p = cfg.providers.find((x) => x.id === pid);
    if (!p) return;
    // 网页版协议要走抓取流程，不是填 Key
    if (p.ptype === "chatgpt_web" || p.ptype === "claude_web") {
      openCapture(pid);
      return;
    }
    keyPid = pid;
    const g = guideFor(p);
    $("keyTitle").textContent = g.title;
    $("keyHint").innerHTML =
      g.html +
      (g.link
        ? `<div class="bm"><a class="key-link" href="${g.link}" target="_blank" rel="noopener">${g.linkText}</a></div>`
        : "");
    if (!keep) {
      $("keyName").value = "";
      $("keySecret").value = "";
    }
    $("keyName").placeholder = `例如：${p.name}-1`;
    $("keySecret").placeholder = g.ph;
    setKeyStatus("", "");
    $("keyForce").classList.add("hidden");
    $("keySave").disabled = false;
    $("keySave").textContent = "验证并添加";
    $("keyModal").classList.remove("hidden");
    setTimeout(() => $("keySecret").focus(), 60);
  }

  function closeKeyModal() {
    $("keyModal").classList.add("hidden");
    keyPid = null;
  }

  /** 粗略识别 Key 归属，用于提示「你可能选错了服务」。 */
  function detectKey(raw) {
    const k = String(raw || "").replace(/\s+/g, "");
    if (/^AIza[0-9A-Za-z_\-]{10,}/.test(k)) return { pid: "gemini", label: "Gemini" };
    if (/^sk-ant-api/.test(k)) return { pid: "claude", label: "Anthropic 官方 Key" };
    if (/^sk-ant-sid/.test(k)) return { pid: "claude", label: "Claude sessionKey" };
    if (/^sk-[A-Za-z0-9]{20,}/.test(k)) return { pid: "deepseek", label: "DeepSeek/OpenAI 风格" };
    return null;
  }

  async function submitKey(force) {
    const p = cfg.providers.find((x) => x.id === keyPid);
    if (!p) return;
    const secret = $("keySecret").value.replace(/\s+/g, "");
    if (!secret) {
      setKeyStatus("请先粘贴 API Key", "err");
      $("keySecret").focus();
      return;
    }
    const name =
      ($("keyName").value || "").trim() ||
      `${p.name}-${(p.accounts || []).length + 1}`;

    const btn = $("keySave");
    btn.disabled = true;
    btn.textContent = "验证中…";
    setKeyStatus("正在向服务端验证 Key…", "pending");

    let v;
    try {
      v = await invoke("verify-account", { providerId: p.id, secret });
    } catch (e) {
      v = { ok: false, error: String((e && e.message) || e) };
    }

    const ok = !!(v && v.ok);
    if (!ok && !force) {
      btn.disabled = false;
      btn.textContent = "验证并添加";
      setKeyStatus("✗ " + escapeHtml(v.error || v.message || "验证失败"), "err");
      $("keyForce").classList.remove("hidden");
      return;
    }

    try {
      await invoke("save-account", {
        providerId: p.id,
        account: {
          name: name,
          cred_type: "api_key",
          secret: secret,
          status: "active",
          priority: (p.accounts || []).length,
        },
      });
      await refreshCfg();
      renderAccounts(p.id);
      renderAccountChip();
      closeKeyModal();
      toast(
        ok ? `已添加并通过验证：${name}` : `已保存（未通过验证）：${name}`,
        ok ? "ok" : "warn"
      );
    } catch (e) {
      btn.disabled = false;
      btn.textContent = "验证并添加";
      setKeyStatus("保存失败：" + escapeHtml(String((e && e.message) || e)), "err");
    }
  }

  async function addAccountByKey() {
    openKeyModal($("acctProviderSel").value);
  }

  // ---------- 网页登录抓取 ----------
  const CHATGPT_BOOKMARKLET =
    "javascript:(async()=>{try{const r=await fetch('/api/auth/session',{credentials:'include'});" +
    "const j=await r.json();if(!j||!j.accessToken){alert('未登录或已过期，请先在 chatgpt.com 登录');return;}" +
    "const data=JSON.stringify({token:j.accessToken,cookie:document.cookie,email:(j.user&&j.user.email)||''});" +
    "await navigator.clipboard.writeText(data);alert('已复制登录信息，回到 AI Hub 粘贴即可');}" +
    "catch(e){alert('抓取失败: '+e);}})()";

  function openCapture(pid) {
    const p = cfg.providers.find((x) => x.id === pid);
    if (!p) return;
    capturePid = pid;
    $("capInput").value = "";
    $("capCookie").value = "";
    $("capStatus").className = "cap-status";
    $("capStatus").textContent = "";
    const isClaude = p.ptype === "claude_web";
    $("capCookieWrap").classList.toggle("hidden", isClaude);
    $("capTitle").textContent = isClaude ? "Claude 网页版登录" : "ChatGPT 网页版登录";

    if (isClaude) {
      $("capInputLabel").textContent = "粘贴 sessionKey";
      $("capInput").placeholder = "从 claude.ai 的 Cookie 中复制 sessionKey 的值";
      $("capSteps").innerHTML =
        "<b>获取 Claude 的 sessionKey：</b><br>" +
        "1. 新标签打开 <code>claude.ai</code> 并登录<br>" +
        "2. 按 <code>F12</code> → <b>Application（应用）</b> → <b>Cookies</b> → <code>https://claude.ai</code><br>" +
        "3. 找到名为 <code>sessionKey</code> 的行，复制其 <b>Value</b><br>" +
        "4. 粘贴到下方，点「验证并添加」<br>" +
        '<span style="opacity:.75">注：sessionKey 是 HttpOnly，脚本读不到，只能手动复制。</span>';
    } else {
      $("capInputLabel").textContent = "粘贴登录信息（书签脚本会自动复制 JSON）";
      $("capInput").placeholder =
        '{"token":"...","cookie":"...","email":"..."} 或直接粘贴 accessToken';
      $("capSteps").innerHTML =
        "<b>获取 ChatGPT 的 accessToken：</b><br>" +
        "1. 把下方按钮<b>拖到书签栏</b>（或右键复制链接存为书签）<br>" +
        "2. 新标签打开 <code>chatgpt.com</code> 并登录<br>" +
        "3. 点击该书签，会自动复制登录信息<br>" +
        "4. 回到本页粘贴，点「验证并添加」<br>" +
        '<div class="bm"><a href="' +
        CHATGPT_BOOKMARKLET.replace(/"/g, "&quot;") +
        '" onclick="return false;">AI Hub 抓取</a>' +
        '<button class="ghost sm" id="capCopyBm">复制脚本</button></div>';
    }
    $("captureModal").classList.remove("hidden");
    const cp = $("capCopyBm");
    if (cp)
      cp.onclick = () => {
        navigator.clipboard.writeText(CHATGPT_BOOKMARKLET).then(
          () => toast("脚本已复制，可存为书签"),
          () => toast("复制失败，请手动拖拽按钮", "warn")
        );
      };
  }

  function closeCapture() {
    $("captureModal").classList.add("hidden");
    capturePid = null;
  }

  function parseCaptureInput() {
    let secret = $("capInput").value.trim();
    let cookie = $("capCookie").value.trim();
    let name = null;
    if (!secret) return null;
    if (secret.startsWith("{")) {
      try {
        const j = JSON.parse(secret);
        if (j.token) secret = String(j.token).trim();
        if (j.cookie && !cookie) cookie = String(j.cookie).trim();
        if (j.email) name = String(j.email).trim();
      } catch (e) {}
    }
    return { secret, cookie: cookie || null, name };
  }

  async function doCapture(verifyOnly) {
    if (!capturePid) return;
    const p = cfg.providers.find((x) => x.id === capturePid);
    const parsed = parseCaptureInput();
    const status = $("capStatus");
    if (!parsed || !parsed.secret) {
      status.className = "cap-status err";
      status.textContent = "请先粘贴 Token / sessionKey";
      return;
    }
    status.className = "cap-status pending";
    status.textContent = "正在验证…";
    $("capAdd").disabled = true;
    $("capVerify").disabled = true;
    try {
      const r = await invoke("verify-account", {
        provider_id: capturePid,
        secret: parsed.secret,
        cookie: parsed.cookie,
      });
      if (!r || r.ok === false) {
        status.className = "cap-status err";
        status.textContent = "验证失败：" + ((r && r.error) || "未知错误");
        return;
      }
      const who = r.email || r.name || "";
      status.className = "cap-status ok";
      status.textContent = "验证通过 ✓" + (who ? "（" + who + "）" : "");
      if (verifyOnly) return;

      const accName =
        parsed.name || who || (p.ptype === "claude_web" ? "Claude 账号" : "ChatGPT 账号");
      const meta =
        p.ptype === "claude_web"
          ? { org_uuid: r.org_uuid }
          : { user_id: r.user_id, email: r.email };
      await invoke("capture-login-token", {
        provider_id: capturePid,
        secret: parsed.secret,
        cookie: parsed.cookie,
        name: accName,
        meta,
      });
      await refreshCfg();
      renderAccounts(capturePid);
      renderAccountChip();
      toast("已添加账号：" + accName);
      closeCapture();
    } catch (e) {
      status.className = "cap-status err";
      status.textContent = "请求失败：" + e;
    } finally {
      $("capAdd").disabled = false;
      $("capVerify").disabled = false;
    }
  }

  async function webLogin() {
    const pid = $("acctProviderSel").value;
    const p = cfg.providers.find((x) => x.id === pid);
    if (!p) return;
    if (p.ptype === "chatgpt_web" || p.ptype === "claude_web") {
      openCapture(pid);
    } else {
      // Key 类端点：直接弹出填 Key 表单（不再只弹一行提示）
      openKeyModal(pid);
    }
  }

  async function toggleDisabled(pid, aid, status) {
    await invoke("set-account-status", { providerId: pid, accountId: aid, status: status === "disabled" ? "active" : "disabled" });
    await refreshCfg();
    renderAccounts(pid);
    renderAccountChip();
  }
  async function togglePreferred(pid, aid) {
    const p = cfg.providers.find((x) => x.id === pid);
    p.preferred_account = p.preferred_account === aid ? null : aid;
    await invoke("save-config", cfg);
    await refreshCfg();
    renderAccounts(pid);
    renderAccountChip();
  }
  async function movePriority(pid, aid, dir) {
    const p = cfg.providers.find((x) => x.id === pid);
    const arr = [...p.accounts].sort((a, b) => a.priority - b.priority);
    const i = arr.findIndex((a) => a.id === aid);
    const j = i + dir;
    if (j < 0 || j >= arr.length) return;
    const tmp = arr[i].priority;
    arr[i].priority = arr[j].priority;
    arr[j].priority = tmp;
    p.accounts = arr;
    await invoke("save-config", cfg);
    await refreshCfg();
    renderAccounts(pid);
  }
  async function delAccount(pid, aid) {
    if (!confirm("删除该账号？")) return;
    await invoke("delete-account", { providerId: pid, accountId: aid });
    await refreshCfg();
    renderAccounts(pid);
    renderAccountChip();
  }
  async function refreshCfg() {
    cfg = await invoke("get-config");
    renderProviders();
    renderAccountChip();
  }

  function saveSettings() {
    invoke("save-config", cfg)
      .then(() => {
        toast("设置已保存");
        closeSettings();
        renderProviders();
        renderModels();
        renderAccountChip();
      })
      .catch((e) => toast("保存失败：" + e, "err"));
  }

  // ---------- 事件绑定 ----------
  function bindUI() {
    $("newChat").onclick = () => newChat();
    $("openSettings").onclick = openSettings;
    $("closeSettings").onclick = closeSettings;
    $("saveSettings").onclick = saveSettings;
    $("openAccounts").onclick = () => {
      openSettings();
      document.querySelector('.tab[data-tab="acct"]').click();
    };
    $("addProvider").onclick = () => {
      const id = "custom-" + Date.now();
      cfg.providers.push({
        id,
        name: "自定义端点",
        ptype: "openai",
        base_url: "https://your-proxy.example/v1",
        api_key: "",
        login_url: null,
        login_token_selector: DEFAULT_SELECTOR,
        models: ["gpt-4o-mini"],
        default_model: "gpt-4o-mini",
        proxy: null,
        enabled: true,
        preferred_account: null,
        accounts: [],
      });
      renderProviderCards();
    };
    $("webLoginBtn").onclick = webLogin;
    $("keyAddBtn").onclick = addAccountByKey;
    $("acctProviderSel").onchange = (e) => renderAccounts(e.target.value);

    // 添加 Key 弹窗
    $("closeKey").onclick = closeKeyModal;
    $("keyCancel").onclick = closeKeyModal;
    $("keySave").onclick = () => submitKey(false);
    $("keyForce").onclick = () => submitKey(true);
    $("keyModal").addEventListener("click", (e) => {
      if (e.target === $("keyModal")) closeKeyModal();
    });
    $("keySecret").addEventListener("input", () => {
      const d = detectKey($("keySecret").value);
      const p = cfg.providers.find((x) => x.id === keyPid);
      if (!d || !p) return;
      if (d.pid !== p.id && cfg.providers.some((x) => x.id === d.pid)) {
        setKeyStatus(
          `这看起来是 ${escapeHtml(d.label)} 的凭据，当前选的是 ${escapeHtml(p.name)}` +
            `<span class="key-switch" id="keySwitch">切换到 ${escapeHtml(d.label)}</span>`,
          "pending"
        );
        const sw = $("keySwitch");
        if (sw) sw.onclick = () => openKeyModal(d.pid, true);
      } else {
        setKeyStatus("", "");
      }
    });
    $("keySecret").addEventListener("keydown", (e) => {
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        submitKey(false);
      }
    });

    $("capVerify").onclick = () => doCapture(true);
    $("capAdd").onclick = () => doCapture(false);
    $("closeCapture").onclick = closeCapture;
    $("captureModal").addEventListener("click", (e) => {
      if (e.target === $("captureModal")) closeCapture();
    });

    $("providerSel").onchange = (e) => {
      curProviderId = e.target.value;
      renderModels();
      renderAccountChip();
    };
    $("modelSel").onchange = (e) => (curModel = e.target.value);

    $("send").onclick = send;
    const input = $("input");
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        send();
      }
    });
    input.addEventListener("input", () => {
      input.style.height = "auto";
      input.style.height = Math.min(input.scrollHeight, 160) + "px";
    });

    document.querySelectorAll(".tab").forEach((t) => {
      t.onclick = () => {
        document.querySelectorAll(".tab").forEach((x) => x.classList.remove("active"));
        t.classList.add("active");
        const tab = t.dataset.tab;
        $("tab-svc").classList.toggle("hidden", tab !== "svc");
        $("tab-acct").classList.toggle("hidden", tab !== "acct");
      };
    });

    $("settingsModal").addEventListener("click", (e) => {
      if (e.target === $("settingsModal")) closeSettings();
    });
  }

  document.addEventListener("DOMContentLoaded", init);
})();
