const API_BASE = "http://127.0.0.1:17654";
const NATIVE_HOST = "com.vortexdl.host";

// Per-tab sniffed media streams: Map<tabId, Map<url, mediaEntry>>
const tabMedia = new Map();
// Recent request headers per URL to pass Referer/User-Agent/Origin/Cookie
const recentHeaders = new Map();
// URLs temporarily bypassed so browser handles them natively if user disables or app is offline
const bypassUrls = new Set();
// Download IDs currently held while deciding whether to hand off to VortexDL
const heldDownloads = new Map();
// Download IDs handed off to VortexDL that must be cancelled & erased in the browser
const cancellingIds = new Set();

const DEFAULT_SETTINGS = {
  interceptDownloads: true,
  minSizeBytes: 512 * 1024, // 512 KB minimum for auto-interception (skip tiny icons/html)
  interceptExtensions: [
    "zip", "rar", "7z", "tar", "gz", "xz", "bz2", "z01", "r00", "001",
    "exe", "msi", "msix", "iso", "img", "dmg", "bin",
    "mp4", "mkv", "webm", "avi", "mov", "flv", "mp3", "flac", "wav", "aac",
    "pdf", "epub", "apk", "torrent"
  ],
};

let cachedSettings = { ...DEFAULT_SETTINGS };

async function getSettings() {
  const stored = await chrome.storage.local.get("vortexSettings");
  cachedSettings = { ...DEFAULT_SETTINGS, ...(stored.vortexSettings || {}) };
  return cachedSettings;
}

const settingsReady = getSettings().catch(() => cachedSettings);

chrome.storage.onChanged.addListener((changes, areaName) => {
  if (areaName === "local" && changes.vortexSettings) {
    cachedSettings = { ...DEFAULT_SETTINGS, ...(changes.vortexSettings.newValue || {}) };
  }
});

function downloadsCall(fn) {
  return new Promise((resolve) => {
    try {
      fn((result) => {
        const err = chrome.runtime.lastError;
        resolve({ ok: !err, error: err ? err.message : null, result });
      });
    } catch (err) {
      resolve({ ok: false, error: String(err), result: undefined });
    }
  });
}

async function addBypassUrl(url) {
  bypassUrls.add(url);
  if (chrome.storage.session) {
    try {
      const data = await chrome.storage.session.get("vortexBypassUrls");
      const list = Array.isArray(data.vortexBypassUrls) ? data.vortexBypassUrls : [];
      if (!list.includes(url)) {
        list.push(url);
        await chrome.storage.session.set({ vortexBypassUrls: list.slice(-50) });
      }
    } catch (_) {}
  }
}

async function consumeBypassUrl(url) {
  if (bypassUrls.has(url)) {
    bypassUrls.delete(url);
    return true;
  }
  if (chrome.storage.session) {
    try {
      const data = await chrome.storage.session.get("vortexBypassUrls");
      const list = Array.isArray(data.vortexBypassUrls) ? data.vortexBypassUrls : [];
      const idx = list.indexOf(url);
      if (idx !== -1) {
        list.splice(idx, 1);
        await chrome.storage.session.set({ vortexBypassUrls: list });
        return true;
      }
    } catch (_) {}
  }
  return false;
}

async function sendToVortex(endpoint, payload) {
  try {
    const resp = await fetch(`${API_BASE}${endpoint}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    if (resp.ok) {
      return await resp.json();
    }
  } catch (_) {
    // Fallback to Native Messaging, which launches vortex-gui.exe if closed
  }

  return new Promise((resolve) => {
    try {
      const action =
        endpoint === "/api/downloads"
          ? "download"
          : endpoint === "/api/media"
          ? "media"
          : endpoint === "/api/grab"
          ? "grab"
          : "ping";
      chrome.runtime.sendNativeMessage(NATIVE_HOST, { action, payload }, (res) => {
        if (chrome.runtime.lastError) {
          resolve({ ok: false, error: chrome.runtime.lastError.message });
        } else {
          resolve(res || { ok: true });
        }
      });
    } catch (err) {
      resolve({ ok: false, error: String(err) });
    }
  });
}

async function collectCookiesHeader(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (cookies && cookies.length > 0) {
      return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    }
  } catch (_) {}
  return "";
}

function isMediaUrlOrContentType(url, contentType) {
  const lowerUrl = url.toLowerCase().split("?")[0];
  const ct = (contentType || "").toLowerCase();
  if (
    lowerUrl.endsWith(".m3u8") ||
    lowerUrl.endsWith(".mpd") ||
    lowerUrl.endsWith(".mp4") ||
    lowerUrl.endsWith(".webm") ||
    lowerUrl.endsWith(".mkv") ||
    lowerUrl.endsWith(".m4a") ||
    lowerUrl.endsWith(".mp3") ||
    lowerUrl.endsWith(".aac") ||
    lowerUrl.endsWith(".flv")
  ) {
    return true;
  }
  if (
    ct.startsWith("video/") ||
    ct.startsWith("audio/") ||
    ct.includes("application/vnd.apple.mpegurl") ||
    ct.includes("application/x-mpegurl") ||
    ct.includes("application/dash+xml")
  ) {
    return true;
  }
  return false;
}

function shouldIgnoreSegment(url) {
  const lower = url.toLowerCase().split("?")[0];
  // Ignore individual HLS/DASH tiny media chunks so the user gets the master .m3u8/.mpd
  return lower.endsWith(".ts") || lower.endsWith(".m4s") || lower.includes("init.mp4");
}

// Capture outgoing headers (Referer, User-Agent, Origin)
chrome.webRequest.onBeforeSendHeaders.addListener(
  (details) => {
    if (!details.url.startsWith("http")) return;
    const hdrs = {};
    for (const h of details.requestHeaders || []) {
      const name = h.name.toLowerCase();
      if (name === "referer" || name === "user-agent" || name === "origin" || name === "cookie") {
        hdrs[h.name] = h.value || "";
      }
    }
    recentHeaders.set(details.url, hdrs);
    if (recentHeaders.size > 400) {
      const firstKey = recentHeaders.keys().next().value;
      recentHeaders.delete(firstKey);
    }
  },
  { urls: ["<all_urls>"] },
  ["requestHeaders"]
);

// Sniff media streams from responses
chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (details.tabId < 0 || !details.url.startsWith("http")) return;
    if (shouldIgnoreSegment(details.url)) return;

    let contentType = "";
    let contentLength = 0;
    for (const h of details.responseHeaders || []) {
      const n = h.name.toLowerCase();
      if (n === "content-type") contentType = h.value || "";
      if (n === "content-length") contentLength = parseInt(h.value || "0", 10) || 0;
    }

    if (!isMediaUrlOrContentType(details.url, contentType)) return;
    // Skip tiny tracking video pixels
    if (contentLength > 0 && contentLength < 32 * 1024 && !details.url.includes(".m3u8") && !details.url.includes(".mpd")) {
      return;
    }

    if (!tabMedia.has(details.tabId)) {
      tabMedia.set(details.tabId, new Map());
    }
    const map = tabMedia.get(details.tabId);
    const isHls = details.url.toLowerCase().includes(".m3u8") || contentType.includes("mpegurl");
    const isDash = details.url.toLowerCase().includes(".mpd") || contentType.includes("dash+xml");

    map.set(details.url, {
      url: details.url,
      contentType,
      sizeBytes: contentLength || null,
      isHls,
      isDash,
      headers: recentHeaders.get(details.url) || {},
      detectedAt: Date.now(),
    });

    const count = map.size;
    chrome.action.setBadgeText({ tabId: details.tabId, text: count > 0 ? String(count) : "" });
    chrome.action.setBadgeBackgroundColor({ tabId: details.tabId, color: "#7C5CFF" });
  },
  { urls: ["<all_urls>"] },
  ["responseHeaders"]
);

chrome.tabs.onRemoved.addListener((tabId) => {
  tabMedia.delete(tabId);
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading" && changeInfo.url) {
    tabMedia.delete(tabId);
    chrome.action.setBadgeText({ tabId, text: "" });
  }
});

function extractDownloadFilename(item) {
  return (item.filename || "").split(/[/\\]/).pop() || "";
}

function isClearlyIgnoredAsset(item) {
  const url = item.finalUrl || item.url || "";
  const filename = extractDownloadFilename(item);
  const extFromName = filename.includes(".") ? filename.split(".").pop().toLowerCase() : "";
  const urlPath = url.toLowerCase().split("?")[0].split("#")[0];
  const lastSeg = urlPath.split("/").pop() || "";
  const extFromUrl = lastSeg.includes(".") ? lastSeg.split(".").pop() : "";
  const ext = extFromName || extFromUrl;
  const ignoredWebExts = ["html", "htm", "css", "js", "mjs", "json", "xml", "svg", "txt"];
  return Boolean(ext && ignoredWebExts.includes(ext));
}

function shouldInterceptDownload(item, settings) {
  const url = item.finalUrl || item.url || "";
  const filename = extractDownloadFilename(item);
  const extFromName = filename.includes(".") ? filename.split(".").pop().toLowerCase() : "";
  const urlPath = url.toLowerCase().split("?")[0].split("#")[0];
  const lastSeg = urlPath.split("/").pop() || "";
  const extFromUrl = lastSeg.includes(".") ? lastSeg.split(".").pop() : "";
  const ext = extFromName || extFromUrl;

  if (isClearlyIgnoredAsset(item)) {
    return false;
  }

  const matchesExt =
    Boolean(ext) &&
    (settings.interceptExtensions.includes(ext) || /^r\d{2}$/.test(ext) || /^\d{3}$/.test(ext));

  const mime = (item.mime || "").toLowerCase();
  const matchesMime =
    mime.startsWith("video/") ||
    mime.startsWith("audio/") ||
    mime.includes("zip") ||
    mime.includes("rar") ||
    mime.includes("7z") ||
    mime.includes("tar") ||
    mime.includes("gzip") ||
    mime.includes("x-bzip2") ||
    mime.includes("x-xz") ||
    mime.includes("x-msdownload") ||
    mime.includes("x-msi") ||
    mime.includes("iso9660") ||
    mime.includes("bittorrent") ||
    mime.includes("android.package-archive") ||
    mime === "application/octet-stream" ||
    mime === "application/pdf" ||
    mime === "application/epub+zip";

  const totalSize = item.fileSize > 0 ? item.fileSize : item.totalBytes > 0 ? item.totalBytes : 0;
  const largeEnough = totalSize > 0 && totalSize >= settings.minSizeBytes;
  return matchesExt || matchesMime || largeEnough;
}

async function buildDownloadHeaders(item, url) {
  const cached = recentHeaders.get(item.finalUrl) || recentHeaders.get(item.url) || recentHeaders.get(url) || {};
  const headers = { ...cached };
  if (item.referrer && !headers.Referer) {
    headers.Referer = item.referrer;
  }
  const cookieStr = await collectCookiesHeader(url);
  if (cookieStr && !headers.Cookie) {
    headers.Cookie = cookieStr;
  }
  return headers;
}

function callSuggestOnce(entry) {
  if (!entry || !entry.suggest || entry.suggested) return;
  entry.suggested = true;
  try {
    entry.suggest();
  } catch (err) {
    console.warn("[VortexDL] suggest failed:", err);
  }
}

async function forceCancelDownload(id, entry) {
  cancellingIds.add(id);
  await downloadsCall((cb) => chrome.downloads.cancel(id, cb));
  await downloadsCall((cb) => chrome.downloads.erase({ id }, cb));

  // Verify the download actually left in_progress; some Chromium states require suggest() before cancel()
  const verifyAndFinish = async () => {
    const res = await downloadsCall((cb) => chrome.downloads.search({ id }, cb));
    const found = Array.isArray(res.result) ? res.result[0] : null;
    if (found && found.state === "in_progress") {
      callSuggestOnce(entry);
      await downloadsCall((cb) => chrome.downloads.cancel(id, cb));
      await downloadsCall((cb) => chrome.downloads.erase({ id }, cb));
    } else if (found) {
      await downloadsCall((cb) => chrome.downloads.erase({ id }, cb));
    }
  };

  await verifyAndFinish();
  setTimeout(() => {
    verifyAndFinish().catch(() => {});
  }, 150);
  setTimeout(() => {
    verifyAndFinish().catch(() => {});
    cancellingIds.delete(id);
  }, 1500);
}

async function releaseHeldDownload(entry) {
  if (!entry || entry.decided) return;
  entry.decided = true;
  if (entry.decisionTimer) {
    clearTimeout(entry.decisionTimer);
    entry.decisionTimer = null;
  }
  heldDownloads.delete(entry.id);
  callSuggestOnce(entry);
  await downloadsCall((cb) => chrome.downloads.resume(entry.id, cb));
}

async function decideHeldDownload(entry, trigger) {
  if (!entry || entry.decided) return;
  if (entry.processing) {
    entry.pendingTrigger = trigger;
    return;
  }
  entry.processing = true;
  try {
    const settings = await settingsReady.then(() => getSettings());
    const searchRes = await downloadsCall((cb) => chrome.downloads.search({ id: entry.id }, cb));
    const latest = Array.isArray(searchRes.result) ? searchRes.result[0] : null;
    if (latest) {
      entry.item = { ...entry.item, ...latest };
    }
    const item = entry.item;
    const url = item.finalUrl || item.url;

    if (!url || !url.startsWith("http") || item.byExtensionId) {
      await releaseHeldDownload(entry);
      return;
    }

    if (await consumeBypassUrl(url)) {
      await releaseHeldDownload(entry);
      return;
    }

    if (!settings.interceptDownloads || isClearlyIgnoredAsset(item)) {
      await releaseHeldDownload(entry);
      return;
    }

    const matched = shouldInterceptDownload(item, settings);
    const hasDetermining = Boolean(chrome.downloads.onDeterminingFilename);
    const hasFilename = Boolean(extractDownloadFilename(item));

    if (matched) {
      // Wait briefly for onDeterminingFilename only if filename isn't known yet on Chromium
      if (trigger === "onCreated" && hasDetermining && !hasFilename && !entry.suggest) {
        if (!entry.decisionTimer) {
          entry.decisionTimer = setTimeout(() => {
            decideHeldDownload(entry, "timeout");
          }, 500);
        }
        return;
      }

      entry.decided = true;
      entry.intercepting = true;
      cancellingIds.add(entry.id);
      if (entry.decisionTimer) {
        clearTimeout(entry.decisionTimer);
        entry.decisionTimer = null;
      }

      const filename = extractDownloadFilename(item);
      const headers = await buildDownloadHeaders(item, url);
      const res = await sendToVortex("/api/downloads", {
        url,
        filename: filename || null,
        headers,
        start_immediately: true,
      });

      heldDownloads.delete(entry.id);
      if (res && res.ok !== false) {
        await forceCancelDownload(entry.id, entry);
      } else {
        cancellingIds.delete(entry.id);
        await addBypassUrl(url);
        callSuggestOnce(entry);
        await downloadsCall((cb) => chrome.downloads.resume(entry.id, cb));
      }
      return;
    }

    // Not matched yet: wait briefly for filename/MIME/fileSize updates before releasing
    if (trigger === "onCreated") {
      if (!entry.decisionTimer) {
        entry.decisionTimer = setTimeout(() => {
          decideHeldDownload(entry, "timeout");
        }, hasDetermining ? 1200 : 800);
      }
      return;
    }

    if (trigger === "onDeterminingFilename" || trigger === "timeout") {
      await releaseHeldDownload(entry);
    }
  } finally {
    entry.processing = false;
    if (entry.pendingTrigger && !entry.decided) {
      const nextTrigger = entry.pendingTrigger;
      entry.pendingTrigger = null;
      decideHeldDownload(entry, nextTrigger);
    }
  }
}

// Universal stage 1: pause synchronously in onCreated on every browser (Edge, Chrome, Brave, Firefox)
chrome.downloads.onCreated.addListener((item) => {
  if (!item || item.byExtensionId) return;
  if (item.state && item.state !== "in_progress") return;
  const url = item.finalUrl || item.url;
  if (!url || !url.startsWith("http")) return;
  if (bypassUrls.has(url)) {
    bypassUrls.delete(url);
    return;
  }
  if (!cachedSettings.interceptDownloads) return;
  if (isClearlyIgnoredAsset(item)) return;

  // Pause immediately before any await so the browser stops pulling bytes right away
  chrome.downloads.pause(item.id, () => {
    void chrome.runtime.lastError;
  });

  const entry = {
    id: item.id,
    item,
    suggest: null,
    suggested: false,
    decided: false,
    processing: false,
    decisionTimer: null,
  };
  heldDownloads.set(item.id, entry);
  decideHeldDownload(entry, "onCreated");
});

// Stage 2 (Chromium/Edge/Brave/Vivaldi): stash suggest() to defer native Save As and capture Content-Disposition
if (chrome.downloads.onDeterminingFilename) {
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    const url = item.finalUrl || item.url;
    if (!url || !url.startsWith("http") || item.byExtensionId) {
      suggest();
      return false;
    }

    let entry = heldDownloads.get(item.id);
    if (entry && (entry.intercepting || entry.decided)) {
      entry.suggest = suggest;
      return true;
    }

    if (cancellingIds.has(item.id)) {
      chrome.downloads.cancel(item.id, () => {
        void chrome.runtime.lastError;
      });
      chrome.downloads.erase({ id: item.id }, () => {
        void chrome.runtime.lastError;
      });
      return true;
    }

    if (!entry) {
      if (bypassUrls.has(url)) {
        bypassUrls.delete(url);
        suggest();
        return false;
      }
      if (!cachedSettings.interceptDownloads || isClearlyIgnoredAsset(item)) {
        suggest();
        return false;
      }
      chrome.downloads.pause(item.id, () => {
        void chrome.runtime.lastError;
      });
      entry = {
        id: item.id,
        item,
        suggest,
        suggested: false,
        decided: false,
        processing: false,
        decisionTimer: null,
      };
      heldDownloads.set(item.id, entry);
    } else {
      entry.item = { ...entry.item, ...item };
      entry.suggest = suggest;
    }

    decideHeldDownload(entry, "onDeterminingFilename");
    return true;
  });
}

// Stage 3: enforce cancellation on any state/attribute change and update metadata on Firefox
chrome.downloads.onChanged.addListener((delta) => {
  if (cancellingIds.has(delta.id)) {
    if (delta.state && delta.state.current === "interrupted") {
      cancellingIds.delete(delta.id);
      chrome.downloads.erase({ id: delta.id }, () => {
        void chrome.runtime.lastError;
      });
      return;
    }
    chrome.downloads.cancel(delta.id, () => {
      void chrome.runtime.lastError;
    });
    return;
  }

  const entry = heldDownloads.get(delta.id);
  if (!entry) return;
  if (delta.filename?.current) {
    entry.item.filename = delta.filename.current;
  }
  if (delta.mime?.current) {
    entry.item.mime = delta.mime.current;
  }
  if (typeof delta.totalBytes?.current === "number") {
    entry.item.totalBytes = delta.totalBytes.current;
  }
  if (delta.filename || delta.mime || delta.totalBytes) {
    decideHeldDownload(entry, "onChanged");
  }
});

// Context menus
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "vortex-download-link",
    title: "Baixar com VortexDL",
    contexts: ["link", "video", "audio", "image"],
  });
  chrome.contextMenus.create({
    id: "vortex-download-media",
    title: "Baixar Vídeo/Mídia (yt-dlp)",
    contexts: ["link", "page", "video"],
  });
  chrome.contextMenus.create({
    id: "vortex-grab-page-links",
    title: "Enviar todos os links da página ao LinkGrabber",
    contexts: ["page", "selection"],
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "vortex-download-link") {
    const targetUrl = info.linkUrl || info.srcUrl;
    if (!targetUrl) return;
    const cookie = await collectCookiesHeader(targetUrl);
    const headers = {};
    if (info.pageUrl) headers.Referer = info.pageUrl;
    if (cookie) headers.Cookie = cookie;
    await sendToVortex("/api/downloads", { url: targetUrl, headers, start_immediately: true });
  } else if (info.menuItemId === "vortex-download-media") {
    const targetUrl = info.linkUrl || info.srcUrl || info.pageUrl;
    if (!targetUrl) return;
    const cookie = await collectCookiesHeader(targetUrl);
    const headers = {};
    if (info.pageUrl) headers.Referer = info.pageUrl;
    if (cookie) headers.Cookie = cookie;
    await sendToVortex("/api/media", {
      url: targetUrl,
      title_hint: tab?.title || null,
      headers,
      is_hls: targetUrl.toLowerCase().includes(".m3u8"),
      is_dash: targetUrl.toLowerCase().includes(".mpd"),
    });
  } else if (info.menuItemId === "vortex-grab-page-links" && tab?.id) {
    chrome.tabs.sendMessage(tab.id, { type: "COLLECT_LINKS", selectionOnly: Boolean(info.selectionText) }, async (res) => {
      if (res?.urls?.length) {
        const cookie = tab.url ? await collectCookiesHeader(tab.url) : "";
        const headers = {};
        if (tab.url) headers.Referer = tab.url;
        if (cookie) headers.Cookie = cookie;
        await sendToVortex("/api/grab", { urls: res.urls, headers });
      }
    });
  }
});

// Messages from popup.js
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  (async () => {
    if (msg.type === "GET_TAB_MEDIA") {
      const map = tabMedia.get(msg.tabId);
      sendResponse({ items: map ? Array.from(map.values()) : [] });
    } else if (msg.type === "SEND_DOWNLOAD") {
      const cookie = await collectCookiesHeader(msg.url);
      const headers = { ...(msg.headers || {}) };
      if (cookie && !headers.Cookie) headers.Cookie = cookie;
      const res = await sendToVortex("/api/downloads", {
        url: msg.url,
        headers,
        start_immediately: true,
      });
      sendResponse(res);
    } else if (msg.type === "SEND_MEDIA") {
      const cookie = await collectCookiesHeader(msg.url);
      const headers = { ...(msg.headers || {}) };
      if (cookie && !headers.Cookie) headers.Cookie = cookie;
      const res = await sendToVortex("/api/media", {
        url: msg.url,
        title_hint: msg.title || null,
        headers,
        is_hls: Boolean(msg.isHls) || msg.url.toLowerCase().includes(".m3u8"),
        is_dash: Boolean(msg.isDash) || msg.url.toLowerCase().includes(".mpd"),
      });
      sendResponse(res);
    } else if (msg.type === "SEND_BATCH_LINKS") {
      const cookie = msg.pageUrl ? await collectCookiesHeader(msg.pageUrl) : "";
      const headers = {};
      if (msg.pageUrl) headers.Referer = msg.pageUrl;
      if (cookie) headers.Cookie = cookie;
      const res = await sendToVortex("/api/grab", { urls: msg.urls, headers });
      sendResponse(res);
    } else if (msg.type === "CHECK_STATUS") {
      try {
        const resp = await fetch(`${API_BASE}/api/ping`);
        if (resp.ok) {
          sendResponse({ online: true, data: await resp.json() });
          return;
        }
      } catch (_) {}
      sendResponse({ online: false });
    }
  })();
  return true;
});
