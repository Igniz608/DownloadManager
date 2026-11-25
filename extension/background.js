const API_BASE = "http://127.0.0.1:17654";
const NATIVE_HOST = "com.vortexdl.host";

// Per-tab sniffed media streams: Map<tabId, Map<url, mediaEntry>>
const tabMedia = new Map();
// Recent request headers per URL to pass Referer/User-Agent/Origin/Cookie
const recentHeaders = new Map();
// URLs temporarily bypassed so browser handles them natively if user disables or app is offline
const bypassUrls = new Set();

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

async function getSettings() {
  const stored = await chrome.storage.local.get("vortexSettings");
  return { ...DEFAULT_SETTINGS, ...(stored.vortexSettings || {}) };
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
        endpoint === "/api/download"
          ? "download"
          : endpoint === "/api/media"
          ? "media"
          : endpoint === "/api/grab/batch"
          ? "grab_batch"
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

// Intercept browser downloads and hand off to VortexDL
chrome.downloads.onCreated.addListener(async (item) => {
  const url = item.finalUrl || item.url;
  if (!url || !url.startsWith("http")) return;
  if (bypassUrls.has(url)) {
    bypassUrls.delete(url);
    return;
  }

  const settings = await getSettings();
  if (!settings.interceptDownloads) return;

  const lowerUrl = url.toLowerCase().split("?")[0];
  const filename = (item.filename || "").split(/[/\\]/).pop() || "";
  const extFromName = filename.includes(".") ? filename.split(".").pop().toLowerCase() : "";
  const extFromUrl = lowerUrl.includes(".") ? lowerUrl.split(".").pop() : "";
  const ext = extFromName || extFromUrl;

  const matchesExt = settings.interceptExtensions.includes(ext) || /^r\d{2}$/.test(ext) || /^\d{3}$/.test(ext);
  const largeEnough = item.fileSize > 0 && item.fileSize >= settings.minSizeBytes;
  if (!matchesExt && !largeEnough) return;

  // Cancel native browser download and pass to VortexDL with cookies & referer
  try {
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch (_) {}

  const headers = { ...(recentHeaders.get(url) || {}) };
  if (item.referrer && !headers.Referer) {
    headers.Referer = item.referrer;
  }
  const cookieStr = await collectCookiesHeader(url);
  if (cookieStr && !headers.Cookie) {
    headers.Cookie = cookieStr;
  }

  const res = await sendToVortex("/api/download", {
    url,
    filename: filename || null,
    headers,
    start_immediately: true,
  });

  if (!res || res.ok === false) {
    // If VortexDL wasn't reachable, fall back cleanly so the user doesn't lose the download
    bypassUrls.add(url);
    chrome.downloads.download({ url });
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
    await sendToVortex("/api/download", { url: targetUrl, headers, start_immediately: true });
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
        await sendToVortex("/api/grab/batch", { urls: res.urls, headers });
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
      const res = await sendToVortex("/api/download", {
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
      const res = await sendToVortex("/api/grab/batch", { urls: msg.urls, headers });
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
