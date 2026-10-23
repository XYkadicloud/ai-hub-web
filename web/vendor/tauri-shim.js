/* tauri-shim：在「本地服务 + 浏览器」模式下模拟 window.__TAURI__，
   把 app.js 里的 invoke() 映射为 fetch()，把 event.listen() 映射为 SSE 分发。
   需在 app.js 之前加载。 */
(function () {
  "use strict";
  var handlers = {};

  function pick(a, b) {
    return a != null ? a : b;
  }

  var TAURI = {
    core: {
      invoke: function (cmd, args) {
        args = args || {};
        return (async function () {
          var method = "GET",
            url,
            body,
            hasBody = false;
          switch (cmd) {
            case "get-config":
              url = "/api/config";
              break;
            case "save-config":
              method = "POST";
              url = "/api/config";
              body = args;
              hasBody = true;
              break;
            case "get-accounts":
              url =
                "/api/accounts?provider_id=" +
                encodeURIComponent(pick(args.provider_id, args.providerId) || "");
              break;
            case "save-account":
              method = "POST";
              url = "/api/accounts";
              body = {
                provider_id: pick(args.provider_id, args.providerId),
                account: args.account,
              };
              hasBody = true;
              break;
            case "delete-account":
              method = "DELETE";
              url =
                "/api/accounts?provider_id=" +
                encodeURIComponent(pick(args.provider_id, args.providerId) || "") +
                "&account_id=" +
                encodeURIComponent(pick(args.account_id, args.accountId) || "");
              break;
            case "set-account-status":
              method = "POST";
              url = "/api/accounts/status";
              body = {
                provider_id: pick(args.provider_id, args.providerId),
                account_id: pick(args.account_id, args.accountId),
                status: args.status,
              };
              hasBody = true;
              break;
            case "capture-login-token":
              method = "POST";
              url = "/api/capture-login-token";
              body = {
                provider_id: pick(args.provider_id, args.providerId),
                secret: args.secret,
                name: args.name,
                cookie: args.cookie,
                meta: args.meta,
              };
              hasBody = true;
              break;
            case "verify-account":
              method = "POST";
              url = "/api/verify-account";
              body = {
                provider_id: pick(args.provider_id, args.providerId),
                secret: args.secret,
                cookie: args.cookie,
              };
              hasBody = true;
              break;
            case "chat-stream":
              method = "POST";
              url = "/api/chat";
              body = {
                provider_id: args.provider_id,
                model: args.model,
                messages: args.messages,
                conv_id: args.conv_id,
                image_mode: !!args.image_mode,
              };
              hasBody = true;
              break;
            case "chat-stop":
              method = "POST";
              url = "/api/chat/stop";
              body = { conv_id: args.conv_id };
              hasBody = true;
              break;
            case "oauth-start":
              method = "POST";
              url = "/api/oauth/start";
              body = { email_hint: args.email_hint };
              hasBody = true;
              break;
            case "oauth-finish":
              method = "POST";
              url = "/api/oauth/finish";
              body = { session_id: args.session_id, callback: args.callback };
              hasBody = true;
              break;
            case "list-conversations":
              url = "/api/conversations";
              break;
            case "create-conversation":
              method = "POST";
              url = "/api/conversations";
              body = {
                id: args.id,
                title: args.title,
                provider: args.provider,
                model: args.model,
              };
              hasBody = true;
              break;
            case "get-conversation":
              url = "/api/conversations/" + encodeURIComponent(args.id);
              break;
            case "delete-conversation":
              method = "DELETE";
              url = "/api/conversations/" + encodeURIComponent(args.id);
              break;
            case "rename-conversation":
              method = "POST";
              url = "/api/conversations/" + encodeURIComponent(args.id) + "/rename";
              body = { title: args.title };
              hasBody = true;
              break;
            case "save-message":
              method = "POST";
              url = "/api/conversations/" + encodeURIComponent(args.id) + "/messages";
              body = {
                role: args.role,
                content: args.content,
                attachments: args.attachments || [],
              };
              hasBody = true;
              break;
            case "open-login-window":
              // 浏览器模式：直接新开标签
              if (args.url) window.open(args.url, "_blank", "noopener");
              return null;
            case "close-login-window":
              return null;
            default:
              throw new Error("未知命令: " + cmd);
          }
          var opt = { method: method, headers: {} };
          if (hasBody) {
            opt.headers["Content-Type"] = "application/json";
            opt.body = JSON.stringify(body);
          }
          var res = await fetch(url, opt);
          if (!res.ok) {
            var t = await res.text();
            throw new Error(t || "HTTP " + res.status);
          }
          var txt = await res.text();
          return txt ? JSON.parse(txt) : null;
        })();
      },
    },
    event: {
      listen: function (name, cb) {
        (handlers[name] = handlers[name] || []).push(cb);
        return Promise.resolve(function () {
          var arr = handlers[name] || [];
          var i = arr.indexOf(cb);
          if (i >= 0) arr.splice(i, 1);
        });
      },
    },
    window: {},
  };

  window.__TAURI__ = TAURI;

  // 文件上传：原始字节 POST（不走 multipart，省依赖也省包体）
  TAURI.core.uploadFile = async function (name, mime, bytes) {
    var url =
      "/api/upload?name=" +
      encodeURIComponent(name) +
      "&mime=" +
      encodeURIComponent(mime || "application/octet-stream");
    var res = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/octet-stream" },
      body: bytes,
    });
    if (!res.ok) {
      var t = await res.text();
      throw new Error(t || "HTTP " + res.status);
    }
    return res.json();
  };

  // 打开 SSE 通道，按事件名分发
  try {
    var es = new EventSource("/api/events");
    es.onmessage = function (e) {
      try {
        var d = JSON.parse(e.data);
        var arr = handlers[d.event] || [];
        for (var i = 0; i < arr.length; i++) {
          try {
            arr[i]({ payload: d.payload });
          } catch (err) {}
        }
      } catch (err) {}
    };
  } catch (err) {}

  // 关闭标签页时通知本地服务退出
  window.addEventListener("pagehide", function () {
    try {
      navigator.sendBeacon("/api/quit");
    } catch (e) {}
  });

  // 心跳（5s）：证明本标签页还活着。服务端收到任何请求都会取消「待退出」，
  // 因此多标签场景下关掉其中一个不会把服务一起带走。
  try {
    setInterval(function () {
      fetch("/api/ping", { method: "POST", cache: "no-store" }).catch(function () {});
    }, 5000);
  } catch (e) {}
})();
