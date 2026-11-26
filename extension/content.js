(() => {
  const MIN_VIDEO_WIDTH = 180;
  const MIN_VIDEO_HEIGHT = 100;

  let overlayEnabled = true;
  let tabHasDetectedMedia = false;
  let rescanScheduled = false;
  let positionScheduled = false;

  const trackedVideos = new Map();

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

  function createOverlayForVideo(video) {
    const host = document.createElement("div");
    host.setAttribute("data-vortexdl-overlay", "1");
    host.style.cssText =
      "position:fixed;z-index:2147483647;margin:0;padding:0;border:0;background:transparent;pointer-events:none;display:none;";

    const shadow = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = `
      :host { all: initial; }
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
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
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
      .vortex-btn:hover {
        opacity: 0.98;
        background: rgba(19, 23, 34, 0.9);
        border-color: rgba(124, 92, 255, 0.95);
        box-shadow: 0 4px 14px rgba(0, 0, 0, 0.38);
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
    `;

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "vortex-btn";
    btn.title = "Baixar vídeo com VortexDL";
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

    const stopBubble = (e) => {
      e.stopPropagation();
    };
    btn.addEventListener("mousedown", stopBubble, true);
    btn.addEventListener("pointerdown", stopBubble, true);
    btn.addEventListener("dblclick", (e) => {
      e.preventDefault();
      e.stopPropagation();
    });

    let resetTimer = null;
    btn.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
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
    });

    shadow.appendChild(style);
    shadow.appendChild(btn);

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

    trackedVideos.set(video, { host, btn, labelEl, ro, onMediaEvent });
    return trackedVideos.get(video);
  }

  function removeOverlay(video) {
    const entry = trackedVideos.get(video);
    if (!entry) return;
    entry.ro.disconnect();
    video.removeEventListener("loadedmetadata", entry.onMediaEvent);
    video.removeEventListener("play", entry.onMediaEvent);
    video.removeEventListener("emptied", entry.onMediaEvent);
    entry.host.remove();
    trackedVideos.delete(video);
  }

  function removeAllOverlays() {
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
        entry.host.style.display = "none";
        continue;
      }

      const rect = video.getBoundingClientRect();
      entry.host.style.display = "block";
      const btnWidth = entry.btn.offsetWidth || 76;
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

  window.addEventListener("scroll", schedulePositionUpdate, { passive: true, capture: true });
  window.addEventListener("resize", schedulePositionUpdate, { passive: true });
  document.addEventListener("fullscreenchange", scheduleRescan);

  scanVideos();
})();
