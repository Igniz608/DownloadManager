use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

#[derive(Debug, Clone)]
enum ResolverEvent {
    IpcMessage(String),
    DownloadIntercepted {
        download_url: String,
    },
    ApiReply {
        ok: bool,
        next_url: Option<String>,
        remaining: usize,
        error: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct JsIpcPayload {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    page_url: String,
    #[serde(default)]
    direct_url: Option<String>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    user_agent: Option<String>,
}

#[derive(Debug, Serialize)]
struct BrowserResolvedRequest {
    page_url: String,
    direct_url: Option<String>,
    filename: Option<String>,
    error: Option<String>,
    headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct BrowserResolvedResponse {
    ok: bool,
    #[serde(default)]
    next_url: Option<String>,
    #[serde(default)]
    remaining: usize,
    #[serde(default)]
    message: Option<String>,
}

const INIT_SCRIPT: &str = r####"
(() => {
  if (window.top !== window) return;
  if (window.__vortexWebviewInit) return;
  window.__vortexWebviewInit = true;

  let resolved = false;
  let sawChallenge = false;
  let autoCaptureTimer = null;

  function cleanUrl() {
    return window.location.href.split("#")[0];
  }

  function isVikingFilePage() {
    if (/(?:^|\.)v[i1]k[i1]ngf[i1]le\.[a-z0-9-]+$/i.test(window.location.hostname || "")) {
      return true;
    }
    const meta = document.querySelector('meta[name="application-name"]');
    if (meta && /viking/i.test(meta.getAttribute("content") || "")) return true;
    return Boolean(document.getElementById("captcha-download") && document.getElementById("download-link"));
  }

  function isChallengePage() {
    const title = (document.title || "").toLowerCase();
    if (title.includes("just a moment") || title.includes("attention required")) return true;
    return Boolean(
      document.getElementById("challenge-running") ||
      document.getElementById("challenge-stage") ||
      document.querySelector(".cf-turnstile, #cf-please-wait, iframe[src*='challenges.cloudflare.com']")
    );
  }

  function sendIpc(obj) {
    try {
      window.ipc.postMessage(JSON.stringify(obj));
    } catch (_) {}
  }

  function ensureBar() {
    if (document.getElementById("vortex-webview-bar")) return document.getElementById("vortex-webview-status");
    const host = document.body || document.documentElement;
    if (!host) return null;

    const bar = document.createElement("div");
    bar.id = "vortex-webview-bar";
    bar.style.cssText = [
      "position:fixed",
      "left:12px",
      "right:12px",
      "bottom:12px",
      "z-index:2147483647",
      "display:flex",
      "align-items:center",
      "justify-content:space-between",
      "gap:10px",
      "padding:10px 14px",
      "border-radius:10px",
      "background:rgba(17,19,28,0.95)",
      "color:#eef1f8",
      "border:1px solid rgba(124,92,255,0.55)",
      "box-shadow:0 8px 24px rgba(0,0,0,0.45)",
      "font-family:system-ui,-apple-system,Segoe UI,sans-serif",
      "font-size:12.5px"
    ].join(";");

    const status = document.createElement("span");
    status.id = "vortex-webview-status";
    status.textContent = "VortexDL: resolva a verificação se aparecer; captura automática";
    status.style.cssText = "flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;";

    const actions = document.createElement("div");
    actions.style.cssText = "display:flex;gap:6px;flex-shrink:0;";

    const mkBtn = (label, bg) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = label;
      btn.style.cssText = [
        `background:${bg}`,
        "color:#fff",
        "border:none",
        "border-radius:6px",
        "padding:6px 10px",
        "font-size:12px",
        "font-weight:600",
        "cursor:pointer"
      ].join(";");
      return btn;
    };

    const captureBtn = mkBtn("Capturar Cookies", "#7c5cff");
    captureBtn.title = "Envia a URL atual e os cookies desta verificação de volta para o VortexDL";
    captureBtn.onclick = () => {
      if (resolved) return;
      resolved = true;
      setStatus("Enviando página e cookies ao VortexDL...");
      sendIpc({
        type: "capture_page",
        page_url: cleanUrl(),
        user_agent: navigator.userAgent
      });
    };

    const skipBtn = mkBtn("Pular", "#374151");
    skipBtn.title = "Pular este link e ir para o próximo";
    skipBtn.onclick = () => {
      if (resolved) return;
      resolved = true;
      setStatus("Pulando para o próximo item...");
      sendIpc({
        type: "error",
        page_url: cleanUrl(),
        error: "Pulado pelo usuário no VortexDL"
      });
    };

    actions.appendChild(captureBtn);
    actions.appendChild(skipBtn);
    bar.appendChild(status);
    bar.appendChild(actions);
    host.appendChild(bar);
    return status;
  }

  function setStatus(msg) {
    const el = ensureBar();
    if (el) el.textContent = msg;
  }

  window.__vortexSetStatus = (msg, allowRetry) => {
    if (allowRetry) resolved = false;
    setStatus(msg);
  };

  function checkPage() {
    ensureBar();
    if (resolved) return;

    const dlEl = document.getElementById("download-link");
    if (dlEl) {
      const rawAttr = (dlEl.getAttribute("href") || "").trim();
      const text = (dlEl.textContent || "").trim();
      const resolvedHref =
        rawAttr && rawAttr !== "#" && !rawAttr.startsWith("javascript:") ? dlEl.href : "";

      if (resolvedHref && /^https?:\/\//i.test(resolvedHref) && !/^Error\s*:/i.test(text)) {
        resolved = true;
        const filename = (document.getElementById("filename")?.textContent || "").trim();
        setStatus("Link capturado! Enviando ao VortexDL...");
        sendIpc({
          type: "resolved",
          page_url: cleanUrl(),
          direct_url: resolvedHref,
          filename: filename || null,
          user_agent: navigator.userAgent
        });
        return;
      }

      if (/^Error\s*:/i.test(text)) {
        resolved = true;
        setStatus(`Erro: ${text}`);
        sendIpc({
          type: "error",
          page_url: cleanUrl(),
          error: text
        });
        return;
      }
    }

    if (!isVikingFilePage()) {
      if (isChallengePage()) {
        sawChallenge = true;
        if (autoCaptureTimer) {
          clearTimeout(autoCaptureTimer);
          autoCaptureTimer = null;
        }
        setStatus("VortexDL: marque a verificação humana acima...");
      } else if (sawChallenge && document.readyState === "complete" && !autoCaptureTimer) {
        setStatus("Verificação concluída! Capturando cookies...");
        autoCaptureTimer = setTimeout(() => {
          if (resolved) return;
          resolved = true;
          sendIpc({
            type: "capture_page",
            page_url: cleanUrl(),
            user_agent: navigator.userAgent
          });
        }, 1200);
      }
    } else if (document.getElementById("captcha-download")) {
      setStatus("VortexDL: marque a verificação do Cloudflare se aparecer...");
    }
  }

  sendIpc({
    type: "hello",
    user_agent: navigator.userAgent
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", checkPage, { once: true });
  }
  if (document.documentElement) {
    new MutationObserver(checkPage).observe(document.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["href", "class"]
    });
  }
  setInterval(checkPage, 350);
  setTimeout(checkPage, 150);
})();
"####;

fn collect_webview_cookies_header(webview: &WebView, urls: &[&str]) -> String {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut pairs: Vec<String> = Vec::new();
    for raw in urls {
        if !raw.starts_with("http://") && !raw.starts_with("https://") {
            continue;
        }
        if let Ok(cookies) = webview.cookies_for_url(raw) {
            for c in cookies {
                let name = c.name().to_string();
                let value = c.value().to_string();
                if seen.insert((name.clone(), value.clone())) {
                    pairs.push(format!("{name}={value}"));
                }
            }
        }
    }
    pairs.join("; ")
}

fn post_browser_resolved(
    port: u16,
    req_body: BrowserResolvedRequest,
    proxy: tao::event_loop::EventLoopProxy<ResolverEvent>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                let _ = proxy.send_event(ResolverEvent::ApiReply {
                    ok: false,
                    next_url: None,
                    remaining: 0,
                    error: Some(format!("Erro ao criar runtime HTTP: {e}")),
                });
                return;
            }
        };

        let result = rt.block_on(async move {
            let client = reqwest::Client::new();
            let endpoint = format!("http://127.0.0.1:{port}/api/browser-resolved");
            let resp = client
                .post(&endpoint)
                .json(&req_body)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                return Ok::<BrowserResolvedResponse, reqwest::Error>(BrowserResolvedResponse {
                    ok: false,
                    next_url: None,
                    remaining: 0,
                    message: Some(format!("HTTP {status}")),
                });
            }
            resp.json::<BrowserResolvedResponse>().await
        });

        match result {
            Ok(data) => {
                let _ = proxy.send_event(ResolverEvent::ApiReply {
                    ok: data.ok,
                    next_url: data.next_url,
                    remaining: data.remaining,
                    error: if data.ok { None } else { data.message },
                });
            }
            Err(e) => {
                let _ = proxy.send_event(ResolverEvent::ApiReply {
                    ok: false,
                    next_url: None,
                    remaining: 0,
                    error: Some(format!("Falha ao comunicar com VortexDL: {e}")),
                });
            }
        }
    });
}

pub fn run_resolver_window(port: u16, initial_url: String) {
    let event_loop = EventLoopBuilder::<ResolverEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = match WindowBuilder::new()
        .with_title("VortexDL — Verificação Humana / Captcha")
        .with_inner_size(LogicalSize::new(620.0, 720.0))
        .with_min_inner_size(LogicalSize::new(420.0, 480.0))
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("Falha ao abrir janela WebView2: {e}");
            return;
        }
    };

    let profile_dir = vortex_core::state::app_data_dir().join("webview_profile");
    let _ = std::fs::create_dir_all(&profile_dir);
    let mut web_context = WebContext::new(Some(profile_dir));

    let ipc_proxy = proxy.clone();
    let dl_proxy = proxy.clone();

    let webview = match WebViewBuilder::new_with_web_context(&mut web_context)
        .with_url(&initial_url)
        .with_initialization_script(INIT_SCRIPT)
        .with_new_window_req_handler(|_url, _features| NewWindowResponse::Deny)
        .with_ipc_handler(move |req| {
            let _ = ipc_proxy.send_event(ResolverEvent::IpcMessage(req.body().clone()));
        })
        .with_download_started_handler(move |download_url, _dest| {
            if download_url.starts_with("http://") || download_url.starts_with("https://") {
                let _ = dl_proxy.send_event(ResolverEvent::DownloadIntercepted { download_url });
                return false;
            }
            true
        })
        .build(&window)
    {
        Ok(wv) => wv,
        Err(e) => {
            tracing::error!("Falha ao inicializar WebView2: {e}");
            return;
        }
    };

    let mut in_flight = false;
    let mut last_user_agent: Option<String> = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(ResolverEvent::IpcMessage(raw)) => {
                let Ok(msg) = serde_json::from_str::<JsIpcPayload>(&raw) else {
                    return;
                };
                if let Some(ref ua) = msg.user_agent {
                    if !ua.trim().is_empty() {
                        last_user_agent = Some(ua.clone());
                    }
                }
                if msg.kind == "hello" {
                    return;
                }
                if in_flight {
                    return;
                }
                in_flight = true;

                let page_url = if msg.page_url.trim().is_empty() {
                    webview
                        .url()
                        .unwrap_or_else(|_| initial_url.clone())
                        .split('#')
                        .next()
                        .unwrap_or("")
                        .to_string()
                } else {
                    msg.page_url
                };

                let (direct_url, error) = match msg.kind.as_str() {
                    "resolved" => (msg.direct_url, None),
                    "capture_page" => (Some(page_url.clone()), None),
                    _ => (
                        None,
                        Some(
                            msg.error
                                .unwrap_or_else(|| "Falha ao resolver no VortexDL".to_string()),
                        ),
                    ),
                };

                let mut headers = HashMap::new();
                if !page_url.is_empty() {
                    headers.insert("Referer".to_string(), page_url.clone());
                }
                if let Some(ua) = msg.user_agent.filter(|s| !s.trim().is_empty()) {
                    headers.insert("User-Agent".to_string(), ua);
                }

                let cookie_urls: Vec<&str> = match direct_url.as_deref() {
                    Some(d) if d != page_url => vec![d, page_url.as_str()],
                    _ => vec![page_url.as_str()],
                };
                let cookie_str = collect_webview_cookies_header(&webview, &cookie_urls);
                if !cookie_str.is_empty() {
                    headers.insert("Cookie".to_string(), cookie_str);
                }

                post_browser_resolved(
                    port,
                    BrowserResolvedRequest {
                        page_url,
                        direct_url,
                        filename: msg.filename,
                        error,
                        headers,
                    },
                    proxy.clone(),
                );
            }
            Event::UserEvent(ResolverEvent::DownloadIntercepted { download_url }) => {
                if in_flight {
                    return;
                }
                in_flight = true;

                let page_url = webview
                    .url()
                    .unwrap_or_else(|_| initial_url.clone())
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .to_string();

                let mut headers = HashMap::new();
                if !page_url.is_empty() {
                    headers.insert("Referer".to_string(), page_url.clone());
                }
                if let Some(ref ua) = last_user_agent {
                    headers.insert("User-Agent".to_string(), ua.clone());
                }
                let cookie_str = collect_webview_cookies_header(
                    &webview,
                    &[download_url.as_str(), page_url.as_str()],
                );
                if !cookie_str.is_empty() {
                    headers.insert("Cookie".to_string(), cookie_str);
                }

                let _ = webview.evaluate_script(
                    "window.__vortexSetStatus && window.__vortexSetStatus('Download interceptado! Enviando ao VortexDL...', false);",
                );

                post_browser_resolved(
                    port,
                    BrowserResolvedRequest {
                        page_url,
                        direct_url: Some(download_url),
                        filename: None,
                        error: None,
                        headers,
                    },
                    proxy.clone(),
                );
            }
            Event::UserEvent(ResolverEvent::ApiReply {
                ok,
                next_url,
                remaining,
                error,
            }) => {
                in_flight = false;
                if let Some(next) = next_url {
                    window.set_title(&format!(
                        "VortexDL — Verificação Humana ({remaining} restante(s))"
                    ));
                    let _ = webview.load_url(&next);
                } else if ok {
                    *control_flow = ControlFlow::Exit;
                } else {
                    let msg = error
                        .unwrap_or_else(|| "Não foi possível enviar ao VortexDL".to_string())
                        .replace('\'', "");
                    let js = format!(
                        "window.__vortexSetStatus && window.__vortexSetStatus('Erro: {msg}', true);"
                    );
                    let _ = webview.evaluate_script(&js);
                }
            }
            _ => {}
        }
    });
}
