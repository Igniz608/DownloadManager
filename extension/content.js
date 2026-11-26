(() => {
  const MIN_VIDEO_WIDTH = 180;
  const MIN_VIDEO_HEIGHT = 100;
  const EDGE_PAD = 10;
  const DRAG_THRESHOLD = 5;
  const DEFAULT_POSITION = { rx: 1, ry: 0 };

  let overlayEnabled = true;
  let overlayOnlyOnHover = false;
  let sitePosition = { ...DEFAULT_POSITION };
  let tabHasDetectedMedia = false;
  let rescanScheduled = false;
  let positionScheduled = false;
  let activeMenuEntry = null;

  const pointerPos = { x: -1, y: -1, active: false };
  const trackedVideos = new Map();

  function clamp(val, min, max) {
    return Math.max(min, Math.min(max, val));
  }

  function clamp01(val) {
    return Number.isFinite(val) ? clamp(val, 0, 1) : 0;
  }

  function getSiteKey() {
    try {
      if (window.top && window.top.location && window.top.location.hostname) {
        return window.top.location.hostname.toLowerCase();
      }
    } catch (_) {}
    if (document.referrer) {
      try {
        const refHost = new URL(document.referrer).hostname.toLowerCase();
        if (refHost) return refHost;
      } catch (_) {}
    }
    return (window.location.hostname || "default").toLowerCase();
  }

  const SITE_KEY = getSiteKey();

  function saveSitePosition(pos) {
    const clean = { rx: clamp01(pos.rx), ry: clamp01(pos.ry) };
    sitePosition = clean;
    chrome.storage.local.get("vortexOverlayPositions", (data) => {
      const map = { ...(data?.vortexOverlayPositions || {}) };
      map[SITE_KEY] = clean;
      chrome.storage.local.set({ vortexOverlayPositions: map });
    });
  }

  function formatBytes(bytes) {
    if (!bytes || bytes <= 0) return "";
    const units = ["B", "KB", "MB", "GB"];
    let v = bytes;
    let i = 0;
    while (v >= 1024 && i < units.length - 1) {
      v /= 1024;
      i++;
    }
    return `${v.toFixed(1)} ${units[i]}`;
  }

  chrome.storage.local.get(["vortexSettings", "vortexOverlayPositions"], (data) => {
    const s = data?.vortexSettings;
    if (s) {
      overlayEnabled = s.showVideoOverlay !== false;
      overlayOnlyOnHover = Boolean(s.overlayOnlyOnHover);
    }
    const savedPos = data?.vortexOverlayPositions?.[SITE_KEY];
    if (savedPos && Number.isFinite(savedPos.rx) && Number.isFinite(savedPos.ry)) {
      sitePosition = { rx: clamp01(savedPos.rx), ry: clamp01(savedPos.ry) };
    }
    if (!overlayEnabled) {
      removeAllOverlays();
    } else {
      schedulePositionUpdate();
    }
  });

  chrome.storage.onChanged.addListener((changes, areaName) => {
    if (areaName !== "local") return;
    if (changes.vortexSettings) {
      const next = changes.vortexSettings.newValue || {};
      overlayEnabled = next.showVideoOverlay !== false;
      overlayOnlyOnHover = Boolean(next.overlayOnlyOnHover);
      if (!overlayEnabled) {
        removeAllOverlays();
      } else {
        scheduleRescan();
      }
    }
    if (changes.vortexOverlayPositions) {
      const nextPos = changes.vortexOverlayPositions.newValue?.[SITE_KEY];
      if (nextPos && Number.isFinite(nextPos.rx) && Number.isFinite(nextPos.ry)) {
        sitePosition = { rx: clamp01(nextPos.rx), ry: clamp01(nextPos.ry) };
        schedulePositionUpdate();
      }
    }
  });

  function collectLinksInDocument(selectionOnly) {
    const allUrls = new Set();
    const priorityUrls = new Set();
    let hasDownloadButtons = false;

    const HOSTER_RE =
      /(?:^|\.)(?:fileditch|fileditchfiles|filekeeper|pixeldrain|gofile|vikingfile|mediafire|buzzheavier|1fichier|mega\.nz|mega\.co\.nz|qiwi\.gg|krakenfiles|akirabox|datanodes|uploadhaven|send\.cm|rapidgator|ddownload|katfile|nitroflare|turbobit|workupload|bowfile|ranoz|anonfiles|bayfiles|dropbox\.com|drive\.google\.com|1drv\.ms|onedrive\.live\.com|huggingface\.co|archive\.org)(?:\/|$|\.)/i;
    const FILE_EXT_RE =
      /\.(?:rar|zip|7z|tar|gz|bz2|xz|part\d+|r\d{2}|z\d{2}|\d{3}|iso|img|exe|msi|apk|dmg|pkg|deb|rpm|torrent|mp4|mkv|avi|mov|webm|m3u8|mpd|mp3|flac|wav|m4a|pdf|epub)(?:[?#]|$)/i;
    const STATIC_ASSET_RE =
      /\.(?:css|js|mjs|cjs|map|png|jpe?g|gif|webp|svg|ico|woff2?|ttf|eot|otf)(?:[?#]|$)/i;
    const DOWNLOAD_TEXT_RE = /\b(?:download|baixar|mirror|part\s*\d+)\b/i;

    const pageSelfBase = `${window.location.origin}${window.location.pathname}${window.location.search}`;

    function resolveCandidate(raw) {
      if (!raw || typeof raw !== "string") return null;
      let cleaned = raw
        .trim()
        .replace(/^["'`(<[]+/, "")
        .replace(/["'`>\];,]+$/, "");
      while (cleaned.endsWith(")")) {
        const opens = (cleaned.match(/\(/g) || []).length;
        const closes = (cleaned.match(/\)/g) || []).length;
        if (closes > opens) {
          cleaned = cleaned.slice(0, -1).replace(/["'`>\];,]+$/, "");
        } else {
          break;
        }
      }
      if (!cleaned || cleaned.startsWith("#")) return null;
      if (/^(?:javascript|mailto|tel|data|blob|about):/i.test(cleaned)) return null;
      if (cleaned.startsWith("magnet:?")) return cleaned;
      try {
        const parsed = new URL(cleaned, window.location.href);
        if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
        const base = `${parsed.origin}${parsed.pathname}${parsed.search}`;
        if (base === pageSelfBase && parsed.hash) return null;
        if (parsed.href === window.location.href) return null;
        return parsed.href;
      } catch (_) {
        return null;
      }
    }

    function isStaticAsset(url) {
      try {
        const pathname = new URL(url).pathname;
        return STATIC_ASSET_RE.test(pathname);
      } catch (_) {
        return false;
      }
    }

    function isDownloadIntent(url, label, forceMedia = false) {
      if (!url) return false;
      if (url.startsWith("magnet:?")) return true;
      if (forceMedia) return true;
      try {
        const u = new URL(url);
        if (HOSTER_RE.test(u.hostname)) return true;
        if (FILE_EXT_RE.test(u.pathname + u.search)) return true;
        if (
          u.hostname === "github.com" &&
          /\/releases\/(?:download|tag)\//i.test(u.pathname)
        ) {
          return true;
        }
        if (label && DOWNLOAD_TEXT_RE.test(label) && !isStaticAsset(url)) {
          return true;
        }
        if (/\/(?:download|dl|getfile)\b/i.test(u.pathname) && !isStaticAsset(url)) {
          return true;
        }
      } catch (_) {}
      return false;
    }

    function addUrl(raw, label = "", forceMedia = false) {
      const resolved = resolveCandidate(raw);
      if (!resolved) return;
      if (!forceMedia && isStaticAsset(resolved)) return;
      allUrls.add(resolved);
      if (isDownloadIntent(resolved, label, forceMedia)) {
        priorityUrls.add(resolved);
      }
    }

    function extractUrlsFromText(text, requireDownloadIntent) {
      if (!text || typeof text !== "string") return;
      const normalized = text
        .replace(/\\\//g, "/")
        .replace(/\\u002[fF]/g, "/");
      const matches = normalized.match(/(?:https?:\/\/|magnet:\?)[^\s"'<>\\`\]}]+/gi) || [];
      for (const m of matches) {
        const resolved = resolveCandidate(m);
        if (!resolved || isStaticAsset(resolved)) continue;
        if (!requireDownloadIntent || isDownloadIntent(resolved, "")) {
          allUrls.add(resolved);
          if (isDownloadIntent(resolved, "")) {
            priorityUrls.add(resolved);
          }
        }
      }
    }

    function getElementLabel(el) {
      return [
        el.innerText,
        el.textContent,
        el.getAttribute?.("aria-label"),
        el.getAttribute?.("title"),
        el.getAttribute?.("download"),
      ]
        .filter(Boolean)
        .join(" ")
        .trim();
    }

    function collectFromRoot(root) {
      if (!root || !root.querySelectorAll) return;

      root.querySelectorAll("a[href], area[href]").forEach((el) => {
        const label = getElementLabel(el);
        if (DOWNLOAD_TEXT_RE.test(label)) hasDownloadButtons = true;
        addUrl(el.getAttribute("href") || el.href, label, el.hasAttribute("download"));
      });

      root
        .querySelectorAll("video[src], video source[src], audio[src], audio source[src]")
        .forEach((el) => {
          addUrl(el.getAttribute("src") || el.src, "media", true);
        });

      root.querySelectorAll("iframe[src]").forEach((el) => {
        addUrl(el.getAttribute("src") || el.src, "iframe", false);
      });

      root
        .querySelectorAll(
          "button, [role='button'], [onclick], form[action], [formaction], input[type='button'], input[type='submit'], [data-url], [data-href], [data-link], [data-download], [data-file], [data-src], [data-clipboard-text]"
        )
        .forEach((el) => {
          const label = getElementLabel(el);
          if (DOWNLOAD_TEXT_RE.test(label) || DOWNLOAD_TEXT_RE.test(el.value || "")) {
            hasDownloadButtons = true;
          }
          if (el.attributes) {
            for (const attr of el.attributes) {
              const name = attr.name.toLowerCase();
              const val = attr.value || "";
              if (!val) continue;
              if (
                name === "href" ||
                name === "action" ||
                name === "formaction" ||
                name === "data-url" ||
                name === "data-href" ||
                name === "data-link" ||
                name === "data-download" ||
                name === "data-file" ||
                name === "data-src" ||
                name === "data-clipboard-text"
              ) {
                addUrl(val, label || "download", DOWNLOAD_TEXT_RE.test(label));
              } else if (name === "onclick" || name.startsWith("data-")) {
                extractUrlsFromText(val, false);
                const relMatch = val.match(/['"]((?:\/|\.\.?\/)[^'"]+)['"]/g) || [];
                for (const rm of relMatch) {
                  addUrl(rm.slice(1, -1), label || "download", DOWNLOAD_TEXT_RE.test(label));
                }
              }
            }
          }
        });

      root.querySelectorAll("*").forEach((el) => {
        if (el.shadowRoot) {
          collectFromRoot(el.shadowRoot);
        }
      });
    }

    if (selectionOnly) {
      const sel = window.getSelection();
      if (sel && sel.rangeCount > 0) {
        const frag = sel.getRangeAt(0).cloneContents();
        collectFromRoot(frag);
        extractUrlsFromText(sel.toString(), false);
        if (allUrls.size > 0) {
          const urls = priorityUrls.size > 0 ? Array.from(priorityUrls) : Array.from(allUrls);
          return { urls, priorityUrls: Array.from(priorityUrls), allUrls: Array.from(allUrls), hasDownloadButtons };
        }
      }
    }

    collectFromRoot(document);

    document.querySelectorAll("script").forEach((s) => {
      if (!s.src && s.textContent && s.textContent.length <= 600000) {
        extractUrlsFromText(s.textContent, true);
      }
    });

    if (document.body?.innerText) {
      extractUrlsFromText(document.body.innerText, true);
    }

    const urls = priorityUrls.size > 0 ? Array.from(priorityUrls) : Array.from(allUrls);
    return {
      urls,
      priorityUrls: Array.from(priorityUrls),
      allUrls: Array.from(allUrls),
      hasDownloadButtons,
    };
  }

  chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (msg.type === "COLLECT_LINKS") {
      sendResponse(collectLinksInDocument(Boolean(msg.selectionOnly)));
      return false;
    }

    if (msg.type === "VORTEX_MEDIA_DETECTED") {
      tabHasDetectedMedia = true;
      scheduleRescan();
    }
    return false;
  });

  function hasPlayableSource(video) {
    if (video.currentSrc || video.src) return true;
    if (video.querySelector("source[src]")) return true;
    if (video.readyState > 0 || (Number.isFinite(video.duration) && video.duration > 0)) {
      return true;
    }
    return tabHasDetectedMedia;
  }

  function resolveVideoSrc(video) {
    if (video.currentSrc) return video.currentSrc;
    if (video.src) return video.src;
    const source = video.querySelector("source[src]");
    return source ? source.src : "";
  }

  function isEligibleVideo(video) {
    if (!overlayEnabled || !video || !video.isConnected) return false;
    if (video.closest("ytd-video-preview, #video-preview, ytd-thumbnail")) return false;

    const fs = document.fullscreenElement;
    if (fs && fs !== video && !fs.contains(video)) return false;

    const style = window.getComputedStyle(video);
    if (
      style.display === "none" ||
      style.visibility === "hidden" ||
      parseFloat(style.opacity || "1") <= 0.05
    ) {
      return false;
    }

    const rect = video.getBoundingClientRect();
    if (rect.width < MIN_VIDEO_WIDTH || rect.height < MIN_VIDEO_HEIGHT) return false;
    if (
      rect.bottom <= 24 ||
      rect.top >= window.innerHeight - 24 ||
      rect.right <= 24 ||
      rect.left >= window.innerWidth - 24
    ) {
      return false;
    }

    return hasPlayableSource(video);
  }

  function closeActiveMenu() {
    if (!activeMenuEntry) return;
    activeMenuEntry.menu.classList.remove("open");
    activeMenuEntry.btn.classList.remove("menu-open");
    activeMenuEntry = null;
  }

  function createOverlayForVideo(video) {
    const host = document.createElement("div");
    host.setAttribute("data-vortexdl-overlay", "1");
    host.style.cssText =
      "position:fixed;z-index:2147483647;margin:0;padding:0;border:0;background:transparent;pointer-events:none;display:none;";

    const shadow = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      :host { all: initial; }
      .wrap {
        position: relative;
        display: inline-flex;
        flex-direction: column;
        align-items: flex-end;
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
        transition: opacity 0.16s ease;
      }
      .wrap.hover-hidden {
        opacity: 0;
        pointer-events: none;
      }
      .wrap.hover-hidden .vortex-btn,
      .wrap.hover-hidden .vortex-menu {
        pointer-events: none;
      }
      .vortex-btn {
        pointer-events: auto;
        touch-action: none;
        display: inline-flex;
        align-items: center;
        gap: 5px;
        height: 27px;
        padding: 0 10px 0 8px;
        border-radius: 999px;
        border: 1px solid rgba(124, 92, 255, 0.48);
        background: rgba(15, 17, 23, 0.54);
        backdrop-filter: blur(8px);
        -webkit-backdrop-filter: blur(8px);
        color: rgba(238, 241, 247, 0.92);
        font-family: inherit;
        font-size: 11.5px;
        font-weight: 600;
        letter-spacing: 0.15px;
        line-height: 1;
        cursor: pointer;
        opacity: 0.56;
        box-shadow: 0 2px 8px rgba(0, 0, 0, 0.24);
        transition: opacity 0.18s ease, background 0.16s ease, border-color 0.16s ease, transform 0.14s ease, box-shadow 0.16s ease;
        user-select: none;
      }
      .vortex-btn:hover,
      .vortex-btn.menu-open,
      .vortex-btn.dragging {
        opacity: 0.98;
        background: rgba(19, 23, 34, 0.92);
        border-color: rgba(124, 92, 255, 0.95);
        box-shadow: 0 4px 14px rgba(0, 0, 0, 0.38);
      }
      .vortex-btn:hover:not(.dragging) {
        transform: translateY(-1px);
      }
      .vortex-btn:active:not(.dragging) {
        transform: translateY(0);
      }
      .vortex-btn.dragging {
        cursor: grabbing;
        transition: none;
      }
      .vortex-btn.busy {
        opacity: 0.94;
        cursor: wait;
      }
      .vortex-btn.ok {
        opacity: 0.98;
        border-color: rgba(60, 208, 112, 0.9);
        background: rgba(18, 36, 26, 0.88);
        color: #7df0a6;
      }
      .vortex-btn.err {
        opacity: 0.98;
        border-color: rgba(239, 77, 96, 0.9);
        background: rgba(42, 18, 24, 0.88);
        color: #ff95a1;
      }
      .vortex-btn svg {
        width: 13px;
        height: 13px;
        flex-shrink: 0;
        stroke: currentColor;
        fill: none;
        stroke-width: 2.2;
        stroke-linecap: round;
        stroke-linejoin: round;
      }
      .vortex-caret {
        width: 10px !important;
        height: 10px !important;
        opacity: 0.78;
        margin-left: 1px;
      }
      .vortex-menu {
        position: absolute;
        top: 33px;
        right: 0;
        pointer-events: auto;
        display: none;
        width: 232px;
        max-height: 320px;
        overflow-y: auto;
        background: rgba(15, 17, 23, 0.94);
        backdrop-filter: blur(12px);
        -webkit-backdrop-filter: blur(12px);
        border: 1px solid rgba(124, 92, 255, 0.45);
        border-radius: 10px;
        padding: 6px;
        box-shadow: 0 10px 28px rgba(0, 0, 0, 0.48);
        color: #eef1f7;
      }
      .vortex-menu.open {
        display: block;
      }
      .vortex-menu.flip-up {
        top: auto;
        bottom: 33px;
      }
      .vortex-menu.align-left {
        left: 0;
        right: auto;
      }
      .menu-header {
        font-size: 11px;
        font-weight: 600;
        color: #9aa3b8;
        padding: 4px 7px 6px;
        white-space: nowrap;
        overflow: hidden;
        text-overflow: ellipsis;
        border-bottom: 1px solid rgba(255, 255, 255, 0.08);
        margin-bottom: 4px;
      }
      .menu-section {
        font-size: 10px;
        font-weight: 700;
        text-transform: uppercase;
        letter-spacing: 0.5px;
        color: #7c87a2;
        padding: 5px 7px 3px;
      }
      .menu-item {
        width: 100%;
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
        background: transparent;
        border: none;
        border-radius: 7px;
        padding: 6px 8px;
        color: #eef1f7;
        font-family: inherit;
        font-size: 12px;
        font-weight: 500;
        text-align: left;
        cursor: pointer;
        transition: background 0.12s ease;
      }
      .menu-item:hover {
        background: rgba(124, 92, 255, 0.22);
      }
      .menu-item-main {
        display: inline-flex;
        align-items: center;
        gap: 6px;
        min-width: 0;
      }
      .menu-item-meta {
        font-size: 10.5px;
        color: #9aa3b8;
        white-space: nowrap;
        flex-shrink: 0;
      }
      .menu-item.audio-item {
        color: #7df0a6;
      }
      .menu-item.audio-item:hover {
        background: rgba(60, 208, 112, 0.16);
      }
      .menu-divider {
        height: 1px;
        background: rgba(255, 255, 255, 0.08);
        margin: 5px 4px;
      }
      .menu-status {
        font-size: 10.5px;
        color: #9aa3b8;
        padding: 4px 7px 2px;
      }
    `;

    const wrap = document.createElement("div");
    wrap.className = "wrap";

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "vortex-btn";
    btn.title = "Escolher qualidade ou áudio para baixar no VortexDL";
    btn.innerHTML = `
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 3v12"></path>
        <path d="m7 10 5 5 5-5"></path>
        <path d="M5 20h14"></path>
      </svg>
    `;
    const labelEl = document.createElement("span");
    labelEl.textContent = "Baixar";
    btn.appendChild(labelEl);

    const caret = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    caret.setAttribute("viewBox", "0 0 24 24");
    caret.setAttribute("aria-hidden", "true");
    caret.setAttribute("class", "vortex-caret");
    caret.innerHTML = `<path d="m6 9 6 6 6-6"></path>`;
    btn.appendChild(caret);

    const menu = document.createElement("div");
    menu.className = "vortex-menu";

    const stopBubble = (e) => {
      e.stopPropagation();
    };
    wrap.addEventListener("mousedown", stopBubble);
    wrap.addEventListener("pointerdown", stopBubble);
    wrap.addEventListener("dblclick", (e) => {
      e.preventDefault();
      e.stopPropagation();
    });

    let resetTimer = null;
    let probeInFlight = false;
    let lastProbeData = null;
    let lastProbeSrc = "";

    function triggerDownload({ formatId = null, qualityLabel = null, audioOnly = false }) {
      closeActiveMenu();
      if (btn.classList.contains("busy")) return;

      if (resetTimer) {
        clearTimeout(resetTimer);
        resetTimer = null;
      }
      btn.classList.remove("ok", "err");
      btn.classList.add("busy");
      labelEl.textContent = "Enviando...";

      chrome.runtime.sendMessage(
        {
          type: "SEND_VIDEO_OVERLAY",
          videoSrc: resolveVideoSrc(video),
          pageUrl: window.location.href,
          frameUrl: window.location.href,
          title: document.title,
          formatId,
          qualityLabel,
          audioOnly,
        },
        (res) => {
          btn.classList.remove("busy");
          const ok = !chrome.runtime.lastError && res?.ok !== false;
          if (ok) {
            btn.classList.add("ok");
            labelEl.textContent = "Enviado!";
          } else {
            btn.classList.add("err");
            labelEl.textContent = "Erro";
          }
          resetTimer = setTimeout(() => {
            btn.classList.remove("ok", "err");
            labelEl.textContent = "Baixar";
          }, 2200);
        }
      );
    }

    function addMenuOption(container, { label, meta = "", isAudio = false, onSelect }) {
      const itemBtn = document.createElement("button");
      itemBtn.type = "button";
      itemBtn.className = isAudio ? "menu-item audio-item" : "menu-item";

      const main = document.createElement("span");
      main.className = "menu-item-main";
      main.textContent = label;
      itemBtn.appendChild(main);

      if (meta) {
        const metaEl = document.createElement("span");
        metaEl.className = "menu-item-meta";
        metaEl.textContent = meta;
        itemBtn.appendChild(metaEl);
      }

      itemBtn.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        onSelect();
      });
      container.appendChild(itemBtn);
    }

    function renderMenu() {
      menu.innerHTML = "";

      const header = document.createElement("div");
      header.className = "menu-header";
      header.textContent = lastProbeData?.title || document.title || "Baixar mídia";
      menu.appendChild(header);

      const videoSec = document.createElement("div");
      videoSec.className = "menu-section";
      videoSec.textContent = "Vídeo + Áudio";
      menu.appendChild(videoSec);

      addMenuOption(menu, {
        label: "Melhor qualidade",
        meta: "Auto",
        onSelect: () => triggerDownload({ qualityLabel: "Melhor qualidade" }),
      });

      const qualities = lastProbeData?.qualities;
      if (Array.isArray(qualities) && qualities.length > 0) {
        for (const q of qualities.slice(0, 7)) {
          const sizeTxt = formatBytes(q.filesize);
          const meta = [q.ext || "mp4", sizeTxt].filter(Boolean).join(" • ");
          addMenuOption(menu, {
            label: q.label,
            meta,
            onSelect: () =>
              triggerDownload({
                formatId: q.format_id,
                qualityLabel: q.label,
                audioOnly: false,
              }),
          });
        }
      } else {
        for (const h of [1080, 720, 480, 360]) {
          addMenuOption(menu, {
            label: `${h}p`,
            meta: "mp4",
            onSelect: () =>
              triggerDownload({
                formatId: `bv*[height<=${h}]+ba/b[height<=${h}]/best`,
                qualityLabel: `${h}p`,
                audioOnly: false,
              }),
          });
        }
      }

      const divider = document.createElement("div");
      divider.className = "menu-divider";
      menu.appendChild(divider);

      const audioSec = document.createElement("div");
      audioSec.className = "menu-section";
      audioSec.textContent = "Somente Áudio";
      menu.appendChild(audioSec);

      addMenuOption(menu, {
        label: "Áudio MP3 (alta qualidade)",
        meta: formatBytes(lastProbeData?.audioSize) || "MP3",
        isAudio: true,
        onSelect: () =>
          triggerDownload({
            qualityLabel: "Áudio MP3",
            audioOnly: true,
          }),
      });

      if (probeInFlight) {
        const statusEl = document.createElement("div");
        statusEl.className = "menu-status";
        statusEl.textContent = "Consultando qualidades disponíveis...";
        menu.appendChild(statusEl);
      }
    }

    function updateMenuAlignment() {
      const btnRect = btn.getBoundingClientRect();
      const spaceBelow = window.innerHeight - (btnRect.bottom + 8);
      menu.classList.toggle("flip-up", spaceBelow < 250 && btnRect.top > 250);
      menu.classList.toggle("align-left", btnRect.right < 244);
    }

    function openMenu() {
      if (activeMenuEntry && activeMenuEntry.menu !== menu) {
        closeActiveMenu();
      }

      updateMenuAlignment();

      const key = `${window.location.href}|${resolveVideoSrc(video)}`;
      if (key !== lastProbeSrc) {
        lastProbeSrc = key;
        lastProbeData = null;
      }

      menu.classList.add("open");
      btn.classList.add("menu-open");
      activeMenuEntry = { host, btn, menu };

      if (!lastProbeData && !probeInFlight) {
        probeInFlight = true;
        renderMenu();
        chrome.runtime.sendMessage(
          {
            type: "PROBE_VIDEO_OVERLAY",
            videoSrc: resolveVideoSrc(video),
            pageUrl: window.location.href,
            frameUrl: window.location.href,
            title: document.title,
          },
          (res) => {
            probeInFlight = false;
            if (!chrome.runtime.lastError && res?.ok) {
              lastProbeData = res;
            }
            if (menu.classList.contains("open")) {
              renderMenu();
            }
          }
        );
      } else {
        renderMenu();
      }
    }

    let suppressNextClick = false;
    let isHostHovered = false;
    let isDragging = false;

    wrap.addEventListener("mouseenter", () => {
      isHostHovered = true;
      const entry = trackedVideos.get(video);
      if (entry) entry.isHostHovered = true;
      schedulePositionUpdate();
    });
    wrap.addEventListener("mouseleave", () => {
      isHostHovered = false;
      const entry = trackedVideos.get(video);
      if (entry) entry.isHostHovered = false;
      schedulePositionUpdate();
    });

    btn.draggable = false;
    btn.title = "Clique para escolher qualidade ou arraste para mover";

    btn.addEventListener("pointerdown", (e) => {
      if (e.button !== 0 || btn.classList.contains("busy")) return;
      e.preventDefault();
      e.stopPropagation();

      const startX = e.clientX;
      const startY = e.clientY;
      const currentRect = btn.getBoundingClientRect();
      const startLeftRaw = parseFloat(host.style.left);
      const startTopRaw = parseFloat(host.style.top);
      const startLeft = Number.isFinite(startLeftRaw) ? startLeftRaw : currentRect.left;
      const startTop = Number.isFinite(startTopRaw) ? startTopRaw : currentRect.top;
      const pointerId = e.pointerId;
      let dragging = false;
      let finished = false;

      const onPointerMove = (moveEvent) => {
        if (moveEvent.pointerId !== undefined && moveEvent.pointerId !== pointerId) return;
        const dx = moveEvent.clientX - startX;
        const dy = moveEvent.clientY - startY;
        if (!dragging) {
          if (Math.hypot(dx, dy) < DRAG_THRESHOLD) return;
          dragging = true;
          isDragging = true;
          const entry = trackedVideos.get(video);
          if (entry) entry.isDragging = true;
          closeActiveMenu();
          btn.classList.add("dragging");
          try {
            btn.setPointerCapture(pointerId);
          } catch (_) {}
        }

        const rect = video.getBoundingClientRect();
        const btnWidth = btn.offsetWidth || 88;
        const btnHeight = btn.offsetHeight || 27;
        const minLeft = rect.left + EDGE_PAD;
        const maxLeft = Math.max(minLeft, rect.right - btnWidth - EDGE_PAD);
        const minTop = rect.top + EDGE_PAD;
        const maxTop = Math.max(minTop, rect.bottom - btnHeight - EDGE_PAD);

        const nextLeft = clamp(startLeft + dx, minLeft, maxLeft);
        const nextTop = clamp(startTop + dy, minTop, maxTop);

        host.style.left = `${Math.round(nextLeft)}px`;
        host.style.top = `${Math.round(nextTop)}px`;

        const rx = maxLeft > minLeft ? clamp01((nextLeft - minLeft) / (maxLeft - minLeft)) : 1;
        const ry = maxTop > minTop ? clamp01((nextTop - minTop) / (maxTop - minTop)) : 0;
        sitePosition = { rx, ry };
      };

      const finishDrag = (upEvent) => {
        if (finished) return;
        if (upEvent?.pointerId !== undefined && upEvent.pointerId !== pointerId) return;
        finished = true;
        btn.removeEventListener("pointermove", onPointerMove);
        btn.removeEventListener("pointerup", finishDrag);
        btn.removeEventListener("pointercancel", finishDrag);
        window.removeEventListener("pointermove", onPointerMove, true);
        window.removeEventListener("pointerup", finishDrag, true);
        window.removeEventListener("pointercancel", finishDrag, true);
        try {
          if (btn.hasPointerCapture(pointerId)) {
            btn.releasePointerCapture(pointerId);
          }
        } catch (_) {}

        if (dragging) {
          suppressNextClick = true;
          setTimeout(() => {
            suppressNextClick = false;
          }, 100);
          isDragging = false;
          const entry = trackedVideos.get(video);
          if (entry) entry.isDragging = false;
          btn.classList.remove("dragging");
          saveSitePosition(sitePosition);
          schedulePositionUpdate();
        }
      };

      btn.addEventListener("pointermove", onPointerMove);
      btn.addEventListener("pointerup", finishDrag);
      btn.addEventListener("pointercancel", finishDrag);
      window.addEventListener("pointermove", onPointerMove, true);
      window.addEventListener("pointerup", finishDrag, true);
      window.addEventListener("pointercancel", finishDrag, true);
    });

    btn.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (suppressNextClick) {
        suppressNextClick = false;
        return;
      }
      if (btn.classList.contains("busy")) return;
      if (menu.classList.contains("open")) {
        closeActiveMenu();
      } else {
        openMenu();
      }
    });

    wrap.appendChild(btn);
    wrap.appendChild(menu);
    shadow.appendChild(style);
    shadow.appendChild(wrap);

    const mountTarget = document.fullscreenElement || document.documentElement;
    if (mountTarget) {
      mountTarget.appendChild(host);
    }

    const onMediaEvent = () => schedulePositionUpdate();
    video.addEventListener("loadedmetadata", onMediaEvent);
    video.addEventListener("play", onMediaEvent);
    video.addEventListener("emptied", onMediaEvent);

    const ro = new ResizeObserver(() => schedulePositionUpdate());
    ro.observe(video);

    trackedVideos.set(video, {
      host,
      wrap,
      btn,
      labelEl,
      menu,
      ro,
      onMediaEvent,
      updateMenuAlignment,
      isHostHovered,
      isDragging,
    });
    return trackedVideos.get(video);
  }

  function removeOverlay(video) {
    const entry = trackedVideos.get(video);
    if (!entry) return;
    if (activeMenuEntry && activeMenuEntry.menu === entry.menu) {
      activeMenuEntry = null;
    }
    entry.ro.disconnect();
    video.removeEventListener("loadedmetadata", entry.onMediaEvent);
    video.removeEventListener("play", entry.onMediaEvent);
    video.removeEventListener("emptied", entry.onMediaEvent);
    entry.host.remove();
    trackedVideos.delete(video);
  }

  function removeAllOverlays() {
    closeActiveMenu();
    for (const video of Array.from(trackedVideos.keys())) {
      removeOverlay(video);
    }
  }

  function isPointerOverRect(rect) {
    return (
      pointerPos.active &&
      pointerPos.x >= rect.left &&
      pointerPos.x <= rect.right &&
      pointerPos.y >= rect.top &&
      pointerPos.y <= rect.bottom
    );
  }

  function updatePositions() {
    positionScheduled = false;
    if (!overlayEnabled) {
      removeAllOverlays();
      return;
    }

    const mountTarget = document.fullscreenElement || document.documentElement;
    for (const [video, entry] of Array.from(trackedVideos.entries())) {
      if (!video.isConnected) {
        removeOverlay(video);
        continue;
      }

      if (mountTarget && entry.host.parentElement !== mountTarget) {
        mountTarget.appendChild(entry.host);
      }

      if (!isEligibleVideo(video)) {
        if (activeMenuEntry && activeMenuEntry.menu === entry.menu) {
          closeActiveMenu();
        }
        entry.host.style.display = "none";
        continue;
      }

      const rect = video.getBoundingClientRect();
      entry.host.style.display = "block";

      if (!entry.isDragging) {
        const btnWidth = entry.btn.offsetWidth || 88;
        const btnHeight = entry.btn.offsetHeight || 27;
        const minLeft = rect.left + EDGE_PAD;
        const maxLeft = Math.max(minLeft, rect.right - btnWidth - EDGE_PAD);
        const minTop = rect.top + EDGE_PAD;
        const maxTop = Math.max(minTop, rect.bottom - btnHeight - EDGE_PAD);

        const rawLeft = minLeft + (maxLeft - minLeft) * clamp01(sitePosition.rx);
        const rawTop = minTop + (maxTop - minTop) * clamp01(sitePosition.ry);

        const top = clamp(rawTop, 8, Math.max(8, window.innerHeight - btnHeight - 8));
        const left = clamp(rawLeft, 8, Math.max(8, window.innerWidth - btnWidth - 8));
        entry.host.style.top = `${Math.round(top)}px`;
        entry.host.style.left = `${Math.round(left)}px`;
      }

      if (entry.menu.classList.contains("open")) {
        entry.updateMenuAlignment();
      }

      const keepVisible =
        !overlayOnlyOnHover ||
        entry.isDragging ||
        entry.menu.classList.contains("open") ||
        entry.btn.classList.contains("busy") ||
        entry.btn.classList.contains("ok") ||
        entry.btn.classList.contains("err") ||
        entry.isHostHovered ||
        isPointerOverRect(rect);

      entry.wrap.classList.toggle("hover-hidden", !keepVisible);
    }
  }

  function scanVideos() {
    rescanScheduled = false;
    if (!overlayEnabled) {
      removeAllOverlays();
      return;
    }

    const videos = document.querySelectorAll("video");
    videos.forEach((video) => {
      if (!trackedVideos.has(video)) {
        createOverlayForVideo(video);
      }
    });
    updatePositions();
  }

  function scheduleRescan() {
    if (rescanScheduled) return;
    rescanScheduled = true;
    setTimeout(scanVideos, 150);
  }

  function schedulePositionUpdate() {
    if (positionScheduled) return;
    positionScheduled = true;
    requestAnimationFrame(updatePositions);
  }

  const observer = new MutationObserver((mutations) => {
    for (const m of mutations) {
      if (m.type === "childList" && (m.addedNodes.length > 0 || m.removedNodes.length > 0)) {
        scheduleRescan();
        return;
      }
      if (m.type === "attributes" && m.target?.nodeName === "VIDEO") {
        schedulePositionUpdate();
        return;
      }
    }
  });

  if (document.documentElement) {
    observer.observe(document.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src", "style", "class"],
    });
  }

  document.addEventListener(
    "pointermove",
    (e) => {
      pointerPos.x = e.clientX;
      pointerPos.y = e.clientY;
      pointerPos.active = true;
      if (overlayOnlyOnHover) {
        schedulePositionUpdate();
      }
    },
    { passive: true, capture: true }
  );

  document.addEventListener("mouseleave", () => {
    pointerPos.active = false;
    if (overlayOnlyOnHover) {
      schedulePositionUpdate();
    }
  });

  window.addEventListener("blur", () => {
    pointerPos.active = false;
    if (overlayOnlyOnHover) {
      schedulePositionUpdate();
    }
  });

  document.addEventListener(
    "pointerdown",
    (e) => {
      if (!activeMenuEntry) return;
      if (e.target === activeMenuEntry.host) return;
      closeActiveMenu();
    },
    true
  );

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && activeMenuEntry) {
      closeActiveMenu();
    }
  });

  window.addEventListener("scroll", schedulePositionUpdate, { passive: true, capture: true });
  window.addEventListener("resize", schedulePositionUpdate, { passive: true });
  document.addEventListener("fullscreenchange", scheduleRescan);

  scanVideos();
})();
