(() => {
  const MIN_VIDEO_WIDTH = 180;
  const MIN_VIDEO_HEIGHT = 100;

  let overlayEnabled = true;
  let tabHasDetectedMedia = false;
  let rescanScheduled = false;
  let positionScheduled = false;
  let activeMenuEntry = null;

  const trackedVideos = new Map();

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

  chrome.storage.local.get("vortexSettings", (data) => {
    const s = data?.vortexSettings;
    if (s && s.showVideoOverlay === false) {
      overlayEnabled = false;
      removeAllOverlays();
    }
  });

  chrome.storage.onChanged.addListener((changes, areaName) => {
    if (areaName === "local" && changes.vortexSettings) {
      const next = changes.vortexSettings.newValue || {};
      overlayEnabled = next.showVideoOverlay !== false;
      if (!overlayEnabled) {
        removeAllOverlays();
      } else {
        scheduleRescan();
      }
    }
  });

  chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
    if (msg.type === "COLLECT_LINKS") {
      const urls = new Set();

      if (msg.selectionOnly) {
        const sel = window.getSelection();
        if (sel && sel.rangeCount > 0) {
          const container = sel.getRangeAt(0).cloneContents();
          container.querySelectorAll("a[href]").forEach((a) => {
            if (a.href && a.href.startsWith("http")) urls.add(a.href);
          });
          const text = sel.toString();
          const matches = text.match(/https?:\/\/[^\s"'<>]+/g) || [];
          matches.forEach((m) => urls.add(m));
        }
      }

      if (urls.size === 0) {
        document.querySelectorAll("a[href]").forEach((a) => {
          if (a.href && a.href.startsWith("http")) urls.add(a.href);
        });
        document
          .querySelectorAll("video[src], video source[src], audio[src], audio source[src]")
          .forEach((el) => {
            if (el.src && el.src.startsWith("http")) urls.add(el.src);
          });
      }

      sendResponse({ urls: Array.from(urls) });
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
      }
      .vortex-btn {
        pointer-events: auto;
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
      .vortex-btn.menu-open {
        opacity: 0.98;
        background: rgba(19, 23, 34, 0.92);
        border-color: rgba(124, 92, 255, 0.95);
        box-shadow: 0 4px 14px rgba(0, 0, 0, 0.38);
      }
      .vortex-btn:hover {
        transform: translateY(-1px);
      }
      .vortex-btn:active {
        transform: translateY(0);
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
        pointer-events: auto;
        display: none;
        margin-top: 6px;
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
        position: absolute;
        bottom: 33px;
        right: 0;
        margin-top: 0;
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
    wrap.addEventListener("mousedown", stopBubble, true);
    wrap.addEventListener("pointerdown", stopBubble, true);
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

    function openMenu() {
      if (activeMenuEntry && activeMenuEntry.menu !== menu) {
        closeActiveMenu();
      }

      const rect = video.getBoundingClientRect();
      const spaceBelow = window.innerHeight - (rect.top + 40);
      menu.classList.toggle("flip-up", spaceBelow < 240 && rect.top > 240);

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

    btn.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
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

    trackedVideos.set(video, { host, btn, labelEl, menu, ro, onMediaEvent });
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
      const btnWidth = entry.btn.offsetWidth || 86;
      const top = Math.max(8, Math.min(window.innerHeight - 36, rect.top + 10));
      const left = Math.max(8, Math.min(window.innerWidth - btnWidth - 8, rect.right - btnWidth - 10));
      entry.host.style.top = `${Math.round(top)}px`;
      entry.host.style.left = `${Math.round(left)}px`;
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
