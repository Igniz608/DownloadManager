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
  showVideoOverlay: true,
  overlayOnlyOnHover: false,
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

function extLog(event, data = {}) {
  const entry = { ts: new Date().toISOString(), event, ...data };
  try {
    fetch(`${API_BASE}/api/ext-log`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(entry),
    }).catch(() => {});
  } catch (_) {}
}

function summarizeItem(item) {
  if (!item) return null;
  return {
    id: item.id,
    url: item.url,
    finalUrl: item.finalUrl,
    filename: item.filename,
    mime: item.mime,
    state: item.state,
    paused: item.paused,
    incognito: item.incognito,
    danger: item.danger,
    bytesReceived: item.bytesReceived,
    totalBytes: item.totalBytes,
    fileSize: item.fileSize,
    byExtensionId: item.byExtensionId,
  };
}

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
    const errBody = await resp.json().catch(() => null);
    extLog("sendToVortex.httpError", { endpoint, status: resp.status, errBody });
    if (resp.status >= 400 && resp.status < 500) {
      return errBody || { ok: false, error: `HTTP ${resp.status}` };
    }
  } catch (err) {
    // Fallback to Native Messaging, which launches vortex-gui.exe if closed
  }

  return new Promise((resolve) => {
    try {
      const action =
        endpoint === "/api/downloads"
          ? "download"
          : endpoint === "/api/media"
          ? "media"
          : endpoint === "/api/media/probe"
          ? "media_probe"
          : endpoint === "/api/grab"
          ? "grab"
          : endpoint === "/api/torrent"
          ? "torrent"
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

async function collectMergedCookiesHeader(urls) {
  const seenNames = new Set();
  const pairs = [];
  for (const url of urls) {
    if (!url || !url.startsWith("http")) continue;
    try {
      const cookies = await chrome.cookies.getAll({ url });
      for (const c of cookies || []) {
        if (!seenNames.has(c.name)) {
          seenNames.add(c.name);
          pairs.push(`${c.name}=${c.value}`);
        }
      }
    } catch (_) {}
  }
  return pairs.join("; ");
}

function normalizeBrowserPageUrl(raw) {
  const trimmed = String(raw || "").trim();
  try {
    const u = new URL(trimmed);
    u.hash = "";
    let s = u.toString();
    if (s.endsWith("/") && u.pathname.length > 1 && !u.search) {
      s = s.slice(0, -1);
    }
    return s;
  } catch (_) {
    return trimmed.split("#")[0].replace(/\/+$/, "");
  }
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
    chrome.tabs.sendMessage(details.tabId, { type: "VORTEX_MEDIA_DETECTED" }).catch(() => {});
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
  const c1 = await downloadsCall((cb) => chrome.downloads.cancel(id, cb));
  const e1 = await downloadsCall((cb) => chrome.downloads.erase({ id }, cb));
  extLog("forceCancel.initial", { id, c1, e1, hasSuggest: Boolean(entry?.suggest), suggested: Boolean(entry?.suggested) });

  // Verify the download actually left in_progress; some Chromium states require suggest() before cancel()
  const verifyAndFinish = async (tag) => {
    const res = await downloadsCall((cb) => chrome.downloads.search({ id }, cb));
    const found = Array.isArray(res.result) ? res.result[0] : null;
    extLog(`forceCancel.verify.${tag}`, { id, searchOk: res.ok, searchErr: res.error, found: summarizeItem(found) });
    if (found && found.state === "in_progress") {
      callSuggestOnce(entry);
      const c2 = await downloadsCall((cb) => chrome.downloads.cancel(id, cb));
      const e2 = await downloadsCall((cb) => chrome.downloads.erase({ id }, cb));
      extLog(`forceCancel.afterSuggest.${tag}`, { id, c2, e2 });
    } else if (found) {
      const e3 = await downloadsCall((cb) => chrome.downloads.erase({ id }, cb));
      extLog(`forceCancel.eraseNonActive.${tag}`, { id, e3 });
    }
  };

  await verifyAndFinish("t0");
  setTimeout(() => {
    verifyAndFinish("t150").catch(() => {});
  }, 150);
  setTimeout(() => {
    verifyAndFinish("t1500").catch(() => {});
  }, 1500);
  setTimeout(() => {
    verifyAndFinish("t10000").catch(() => {});
    cancellingIds.delete(id);
  }, 10000);
}

async function releaseHeldDownload(entry, reason = "unmatched") {
  if (!entry || entry.decided) return;
  entry.decided = true;
  if (entry.decisionTimer) {
    clearTimeout(entry.decisionTimer);
    entry.decisionTimer = null;
  }
  heldDownloads.delete(entry.id);
  callSuggestOnce(entry);
  const r = await downloadsCall((cb) => chrome.downloads.resume(entry.id, cb));
  extLog("decide.release", { id: entry.id, reason, r, item: summarizeItem(entry.item) });
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
      extLog("decide.sendToVortex", { id: entry.id, trigger, matched, filename, res });

      heldDownloads.delete(entry.id);
      if (res && res.ok !== false) {
        await forceCancelDownload(entry.id, entry);
      } else {
        cancellingIds.delete(entry.id);
        await addBypassUrl(url);
        callSuggestOnce(entry);
        const r = await downloadsCall((cb) => chrome.downloads.resume(entry.id, cb));
        extLog("decide.fallbackResume", { id: entry.id, r });
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
      await releaseHeldDownload(entry, trigger);
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
  extLog("downloads.onCreated", { item: summarizeItem(item), interceptDownloads: cachedSettings.interceptDownloads });
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
    const err = chrome.runtime.lastError;
    if (err) extLog("onCreated.pauseError", { id: item.id, error: err.message });
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
    extLog("downloads.onDeterminingFilename", { item: summarizeItem(item), hasHeld: heldDownloads.has(item?.id), isCancelling: cancellingIds.has(item?.id) });
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
        const err = chrome.runtime.lastError;
        if (err) extLog("onDeterminingFilename.cancelError", { id: item.id, error: err.message });
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
    extLog("downloads.onChanged.cancelling", { delta });
    if (delta.state && delta.state.current === "interrupted") {
      cancellingIds.delete(delta.id);
      chrome.downloads.erase({ id: delta.id }, () => {
        void chrome.runtime.lastError;
      });
      return;
    }
    chrome.downloads.cancel(delta.id, () => {
      const err = chrome.runtime.lastError;
      if (err) extLog("onChanged.cancelError", { id: delta.id, error: err.message });
    });
    return;
  }

  const entry = heldDownloads.get(delta.id);
  if (!entry) return;
  extLog("downloads.onChanged.held", { delta });
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
    if (/^magnet:/i.test(targetUrl.trim())) {
      await sendToVortex("/api/torrent", { source: targetUrl.trim() });
      return;
    }
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
    const collected = await collectAllTabLinks(tab.id, Boolean(info.selectionText));
    if (collected.urls?.length) {
      const cookie = tab.url ? await collectCookiesHeader(tab.url) : "";
      const headers = {};
      if (tab.url) headers.Referer = tab.url;
      if (cookie) headers.Cookie = cookie;
      await sendToVortex("/api/grab", { urls: collected.urls, headers });
    }
  }
});

function collectLinksFromFrame(selectionOnly) {
  const allUrls = new Set();
  const priorityUrls = new Set();
  let hasDownloadButtons = false;

  const HOSTER_RE =
    /(?:^|\.)(?:fileditch|fileditchfiles|fileditchvids|itsfileditchfiles|fileditchhot|theditch|embedditch|filekeeper|pixeldrain|gofile|vikingfile|vik1ngfile|mediafire|buzzheavier|bzzhr|1fichier|mega\.nz|mega\.co\.nz|qiwi\.gg|krakenfiles|akirabox|datanodes|uploadhaven|send\.cm|rapidgator|ddownload|katfile|nitroflare|turbobit|workupload|bowfile|ranoz|rootz|fuckingfast|anonfiles|bayfiles|dropbox\.com|drive\.google\.com|1drv\.ms|onedrive\.live\.com|huggingface\.co|archive\.org)(?:\/|$|\.)/i;
  const BARE_HOSTER_RE =
    /(?:^|[^a-zA-Z0-9._:/@])((?:fileditchfiles\.(?:me|st)|itsfileditchfiles\.st|fileditchvids\.st|fileditchhot\.st|theditch\.st|embedditch\.st|filekeeper\.net|pixeldrain\.com|gofile\.io|vikingfile\.com|vik1ngfile\.site|buzzheavier\.com|bzzhr\.co|mediafire\.com|1fichier\.com|mega\.nz|akirabox\.com|datanodes\.to)\/[A-Za-z0-9_/?=&.%#+-]+)/gi;
  const HOST_ATTR_HINT_RE =
    /https?:|magnet:|\\\/|\\u002f|aHR0c|fileditch|theditch|filekeeper|pixeldrain|gofile|vikingfile|vik1ngfile|buzzheavier|bzzhr|mediafire|1fichier|mega\.nz|akirabox|datanodes/i;
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

  function extractUrlsFromText(text, requireDownloadIntent, depth = 0) {
    if (!text || typeof text !== "string") return;
    const normalized = text
      .replace(/\\\//g, "/")
      .replace(/\\u002[fF]/g, "/")
      .replace(/\\x2[fF]/g, "/")
      .replace(/\\u003[aA]/g, ":")
      .replace(/\\x3[aA]/g, ":")
      .replace(/\\u0026/g, "&")
      .replace(/&amp;/g, "&");
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
    BARE_HOSTER_RE.lastIndex = 0;
    let bareMatch;
    while ((bareMatch = BARE_HOSTER_RE.exec(normalized)) !== null) {
      const resolved = resolveCandidate(`https://${bareMatch[1]}`);
      if (!resolved || isStaticAsset(resolved)) continue;
      allUrls.add(resolved);
      priorityUrls.add(resolved);
    }
    if (depth === 0) {
      const b64Matches = normalized.match(/aHR0c(?:Dov|HM6)[A-Za-z0-9+/_-]{8,}={0,2}/g) || [];
      for (const b64 of b64Matches.slice(0, 40)) {
        try {
          let std = b64.replace(/-/g, "+").replace(/_/g, "/");
          while (std.length % 4 !== 0) std += "=";
          const decoded = atob(std);
          if (/^https?:\/\//i.test(decoded)) {
            extractUrlsFromText(decoded, requireDownloadIntent, 1);
          }
        } catch (_) {}
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
      if (el.attributes) {
        for (const attr of el.attributes) {
          const name = attr.name.toLowerCase();
          if (
            name.startsWith("data-") ||
            name === "x-data" ||
            name === "x-init" ||
            name === "ng-click" ||
            name === "v-on:click" ||
            name === "@click"
          ) {
            const val = attr.value || "";
            if (val && HOST_ATTR_HINT_RE.test(val)) {
              extractUrlsFromText(val, false);
            }
          }
        }
      }
      if (el.tagName === "TEMPLATE" && el.content) {
        collectFromRoot(el.content);
      }
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
        return {
          urls: priorityUrls.size > 0 ? Array.from(priorityUrls) : Array.from(allUrls),
          priorityUrls: Array.from(priorityUrls),
          allUrls: Array.from(allUrls),
          hasDownloadButtons,
          scriptUrls: [],
        };
      }
    }
  }

  collectFromRoot(document);

  const scriptUrls = [];
  const seenScriptUrls = new Set();
  const VENDOR_SCRIPT_RE =
    /(?:jquery|bootstrap|tailwind|lucide|fontawesome|gtag|analytics|turnstile|recaptcha|cloudflare|adsbygoogle|popper|alpinejs)/i;
  document.querySelectorAll("script").forEach((s) => {
    if (!s.src && s.textContent && s.textContent.length <= 1500000) {
      extractUrlsFromText(s.textContent, true);
    } else if (s.src) {
      try {
        const su = new URL(s.src, window.location.href);
        if (
          su.origin === window.location.origin &&
          !VENDOR_SCRIPT_RE.test(su.pathname) &&
          !seenScriptUrls.has(su.href) &&
          scriptUrls.length < 12
        ) {
          seenScriptUrls.add(su.href);
          scriptUrls.push(su.href);
        }
      } catch (_) {}
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
    scriptUrls,
  };
}

function collectMainWorldStateUrls() {
  const candidates = [];
  const globals = ["__NEXT_DATA__", "__NUXT__", "__DATA__", "__INITIAL_STATE__", "__APP_STATE__"];
  for (const key of globals) {
    try {
      const val = window[key];
      if (val && typeof val === "object") {
        const json = JSON.stringify(val);
        if (json && json.length <= 1000000) {
          candidates.push(json);
        }
      }
    } catch (_) {}
  }
  return candidates.join("\n");
}

function extractDownloadUrlsFromBlobText(text) {
  if (!text || typeof text !== "string") return [];
  const HOSTER_RE =
    /(?:^|\.)(?:fileditch|fileditchfiles|fileditchvids|itsfileditchfiles|fileditchhot|theditch|embedditch|filekeeper|pixeldrain|gofile|vikingfile|vik1ngfile|mediafire|buzzheavier|bzzhr|1fichier|mega\.nz|mega\.co\.nz|qiwi\.gg|krakenfiles|akirabox|datanodes|uploadhaven|send\.cm|rapidgator|ddownload|katfile|nitroflare|turbobit|workupload|bowfile|ranoz|rootz|fuckingfast|anonfiles|bayfiles|dropbox\.com|drive\.google\.com|1drv\.ms|onedrive\.live\.com|huggingface\.co|archive\.org)(?:\/|$|\.)/i;
  const FILE_EXT_RE =
    /\.(?:rar|zip|7z|tar|gz|bz2|xz|part\d+|r\d{2}|z\d{2}|\d{3}|iso|img|exe|msi|apk|dmg|pkg|deb|rpm|torrent|mp4|mkv|avi|mov|webm|m3u8|mpd|mp3|flac|wav|m4a|pdf|epub)(?:[?#]|$)/i;
  const BARE_HOSTER_RE =
    /(?:^|[^a-zA-Z0-9._:/@])((?:fileditchfiles\.(?:me|st)|itsfileditchfiles\.st|fileditchvids\.st|fileditchhot\.st|theditch\.st|embedditch\.st|filekeeper\.net|pixeldrain\.com|gofile\.io|vikingfile\.com|vik1ngfile\.site|buzzheavier\.com|bzzhr\.co|mediafire\.com|1fichier\.com|mega\.nz|akirabox\.com|datanodes\.to)\/[A-Za-z0-9_/?=&.%#+-]+)/gi;

  const found = new Set();
  const normalized = text
    .replace(/\\\//g, "/")
    .replace(/\\u002[fF]/g, "/")
    .replace(/\\x2[fF]/g, "/")
    .replace(/\\u003[aA]/g, ":")
    .replace(/\\x3[aA]/g, ":")
    .replace(/\\u0026/g, "&")
    .replace(/&amp;/g, "&");

  const scan = (src) => {
    const matches = src.match(/(?:https?:\/\/|magnet:\?)[^\s"'<>\\`\]}]+/gi) || [];
    for (const raw of matches) {
      const cleaned = raw.trim().replace(/^["'`(<[]+/, "").replace(/["'`>\];,)]+$/, "");
      if (cleaned.startsWith("magnet:?")) {
        found.add(cleaned);
        continue;
      }
      try {
        const u = new URL(cleaned);
        if (HOSTER_RE.test(u.hostname) || FILE_EXT_RE.test(u.pathname + u.search)) {
          found.add(u.href);
        }
      } catch (_) {}
    }
    BARE_HOSTER_RE.lastIndex = 0;
    let m;
    while ((m = BARE_HOSTER_RE.exec(src)) !== null) {
      try {
        found.add(new URL(`https://${m[1]}`).href);
      } catch (_) {}
    }
  };

  scan(normalized);
  const b64Matches = normalized.match(/aHR0c(?:Dov|HM6)[A-Za-z0-9+/_-]{8,}={0,2}/g) || [];
  for (const b64 of b64Matches.slice(0, 40)) {
    try {
      let std = b64.replace(/-/g, "+").replace(/_/g, "/");
      while (std.length % 4 !== 0) std += "=";
      const decoded = atob(std);
      if (/^https?:\/\//i.test(decoded)) scan(decoded);
    } catch (_) {}
  }
  return Array.from(found);
}

async function collectAllTabLinks(tabId, selectionOnly = false) {
  const mergedPriority = new Set();
  const mergedAll = new Set();
  const scriptUrls = new Set();
  let hasDownloadButtons = false;

  try {
    const frames = await chrome.scripting.executeScript({
      target: { tabId, allFrames: true },
      func: collectLinksFromFrame,
      args: [Boolean(selectionOnly)],
    });
    for (const frame of frames || []) {
      const res = frame?.result;
      if (!res) continue;
      if (res.hasDownloadButtons) hasDownloadButtons = true;
      (res.priorityUrls || []).forEach((u) => mergedPriority.add(u));
      (res.allUrls || res.urls || []).forEach((u) => mergedAll.add(u));
      (res.scriptUrls || []).forEach((u) => scriptUrls.add(u));
    }
    if (!selectionOnly) {
      try {
        const mainFrames = await chrome.scripting.executeScript({
          target: { tabId, allFrames: false },
          world: "MAIN",
          func: collectMainWorldStateUrls,
        });
        for (const mf of mainFrames || []) {
          if (mf?.result) {
            extractDownloadUrlsFromBlobText(mf.result).forEach((u) => {
              mergedPriority.add(u);
              mergedAll.add(u);
            });
          }
        }
      } catch (_) {}

      const toFetch = Array.from(scriptUrls).slice(0, 12);
      if (toFetch.length > 0) {
        const texts = await Promise.all(
          toFetch.map(async (su) => {
            try {
              const resp = await fetch(su, { signal: AbortSignal.timeout(3000) });
              if (!resp.ok) return "";
              const txt = await resp.text();
              return txt.length <= 1500000 ? txt : "";
            } catch (_) {
              return "";
            }
          })
        );
        for (const txt of texts) {
          extractDownloadUrlsFromBlobText(txt).forEach((u) => {
            mergedPriority.add(u);
            mergedAll.add(u);
          });
        }
      }
    }
    const urls = mergedPriority.size > 0 ? Array.from(mergedPriority) : Array.from(mergedAll);
    return { urls, hasDownloadButtons };
  } catch (err) {
    return await new Promise((resolve) => {
      chrome.tabs.sendMessage(
        tabId,
        { type: "COLLECT_LINKS", selectionOnly: Boolean(selectionOnly) },
        (res) => {
          const sendErr = chrome.runtime.lastError;
          if (sendErr || !res) {
            resolve({
              urls: [],
              hasDownloadButtons: false,
              error: err?.message || sendErr?.message || "Script execution failed",
            });
          } else {
            resolve({
              urls: res.urls || [],
              hasDownloadButtons: Boolean(res.hasDownloadButtons),
            });
          }
        }
      );
    });
  }
}

function isKnownVideoPlatformUrl(rawUrl) {
  try {
    const host = new URL(rawUrl).hostname.toLowerCase();
    return (
      host === "youtube.com" ||
      host.endsWith(".youtube.com") ||
      host === "youtu.be" ||
      host === "twitch.tv" ||
      host.endsWith(".twitch.tv") ||
      host === "vimeo.com" ||
      host.endsWith(".vimeo.com") ||
      host === "tiktok.com" ||
      host.endsWith(".tiktok.com") ||
      host === "instagram.com" ||
      host.endsWith(".instagram.com") ||
      host === "x.com" ||
      host === "twitter.com" ||
      host.endsWith(".twitter.com") ||
      host === "reddit.com" ||
      host.endsWith(".reddit.com") ||
      host === "facebook.com" ||
      host.endsWith(".facebook.com") ||
      host === "fb.watch" ||
      host === "dailymotion.com" ||
      host.endsWith(".dailymotion.com") ||
      host === "kick.com" ||
      host === "bilibili.com" ||
      host.endsWith(".bilibili.com")
    );
  } catch (_) {
    return false;
  }
}

const probeCache = new Map();

async function resolveOverlayMediaTarget(msg, sender) {
  const tabUrl = sender?.tab?.url || msg.pageUrl || "";
  const frameUrl = msg.frameUrl || tabUrl;
  const title = sender?.tab?.title || msg.title || null;
  const tabId = sender?.tab?.id;
  const videoSrc = (msg.videoSrc || "").trim();

  let targetUrl = tabUrl || frameUrl;
  let targetHeaders = { Referer: tabUrl || frameUrl };
  let isHls = false;
  let isDash = false;

  const usePageExtractor = isKnownVideoPlatformUrl(tabUrl) || isKnownVideoPlatformUrl(frameUrl);
  if (usePageExtractor) {
    targetUrl = isKnownVideoPlatformUrl(tabUrl) ? tabUrl : frameUrl;
  } else if (videoSrc.startsWith("http://") || videoSrc.startsWith("https://")) {
    targetUrl = videoSrc;
    targetHeaders = { ...(recentHeaders.get(videoSrc) || {}), Referer: tabUrl || frameUrl };
    isHls = videoSrc.toLowerCase().includes(".m3u8");
    isDash = videoSrc.toLowerCase().includes(".mpd");
  } else if (tabId != null && tabMedia.has(tabId)) {
    const items = Array.from(tabMedia.get(tabId).values());
    const preferred =
      items.slice().reverse().find((i) => i.isHls || i.isDash) ||
      items[items.length - 1];
    if (preferred) {
      targetUrl = preferred.url;
      targetHeaders = { ...(preferred.headers || {}), Referer: tabUrl || frameUrl };
      isHls = Boolean(preferred.isHls);
      isDash = Boolean(preferred.isDash);
    }
  }

  const cookie = await collectCookiesHeader(targetUrl);
  if (cookie && !targetHeaders.Cookie) targetHeaders.Cookie = cookie;

  return {
    targetUrl,
    targetHeaders,
    title,
    isHls: isHls || targetUrl.toLowerCase().includes(".m3u8"),
    isDash: isDash || targetUrl.toLowerCase().includes(".mpd"),
  };
}

function summarizeProbeFormats(probeResult) {
  const formats = Array.isArray(probeResult?.formats) ? probeResult.formats : [];
  const byHeight = new Map();
  let bestAudio = null;

  for (const f of formats) {
    if (f.is_audio_only) {
      if (!bestAudio || (f.filesize || 0) > (bestAudio.filesize || 0)) {
        bestAudio = f;
      }
      continue;
    }
    const res = String(f.resolution || "");
    const m = res.match(/(\d+)x(\d+)|(\d+)p/);
    const height = m ? parseInt(m[2] || m[3] || "0", 10) : 0;
    if (height <= 0) continue;

    const ext = String(f.ext || "mp4").toLowerCase();
    const vcodec = String(f.vcodec || "").toLowerCase();
    const score =
      (ext === "mp4" ? 100 : 0) +
      (vcodec.startsWith("avc1") ? 40 : vcodec.startsWith("vp") ? 20 : 10) +
      (f.filesize ? 5 : 0);

    const prev = byHeight.get(height);
    if (!prev || score > prev._score || (score === prev._score && (f.filesize || 0) > (prev.filesize || 0))) {
      byHeight.set(height, {
        format_id: f.format_id,
        height,
        label: `${height}p`,
        ext,
        filesize: f.filesize || null,
        _score: score,
      });
    }
  }

  const qualities = Array.from(byHeight.values())
    .sort((a, b) => b.height - a.height)
    .map(({ _score, ...rest }) => rest);

  return {
    title: probeResult?.title || null,
    qualities,
    audioSize: bestAudio?.filesize || null,
  };
}

// Messages from popup.js and content.js
chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
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
    } else if (msg.type === "SEND_TORRENT") {
      const source = typeof msg.source === "string" ? msg.source.trim() : "";
      if (!source) {
        sendResponse({ ok: false, error: "Fonte de torrent vazia" });
        return;
      }
      const res = await sendToVortex("/api/torrent", { source });
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
    } else if (msg.type === "PROBE_VIDEO_OVERLAY") {
      const target = await resolveOverlayMediaTarget(msg, sender);
      const cached = probeCache.get(target.targetUrl);
      if (cached && Date.now() - cached.ts < 60000) {
        sendResponse({ ok: true, ...cached.summary });
        return;
      }

      const raw = await sendToVortex("/api/media/probe", {
        url: target.targetUrl,
        headers: target.targetHeaders,
        single_video: true,
      });
      const probeResult = raw?.data?.result || raw?.result;
      const ok = Boolean((raw?.data?.ok ?? raw?.ok) && probeResult);
      if (!ok) {
        sendResponse({ ok: false, error: raw?.data?.message || raw?.message || raw?.error || "Falha ao consultar formatos" });
        return;
      }
      const summary = summarizeProbeFormats(probeResult);
      probeCache.set(target.targetUrl, { ts: Date.now(), summary });
      if (probeCache.size > 40) {
        probeCache.delete(probeCache.keys().next().value);
      }
      sendResponse({ ok: true, ...summary });
    } else if (msg.type === "SEND_VIDEO_OVERLAY") {
      const target = await resolveOverlayMediaTarget(msg, sender);
      const res = await sendToVortex("/api/media", {
        url: target.targetUrl,
        title_hint: target.title,
        format_id: msg.formatId || null,
        quality_label: msg.qualityLabel || null,
        audio_only: Boolean(msg.audioOnly),
        headers: target.targetHeaders,
        is_hls: target.isHls,
        is_dash: target.isDash,
      });
      sendResponse(res);
    } else if (msg.type === "COLLECT_TAB_LINKS") {
      const res = await collectAllTabLinks(msg.tabId, Boolean(msg.selectionOnly));
      sendResponse(res);
    } else if (msg.type === "SEND_BATCH_LINKS") {
      const cookie = msg.pageUrl ? await collectCookiesHeader(msg.pageUrl) : "";
      const headers = {};
      if (msg.pageUrl) headers.Referer = msg.pageUrl;
      if (cookie) headers.Cookie = cookie;
      const res = await sendToVortex("/api/grab", { urls: msg.urls, headers });
      sendResponse(res);
    } else if (msg.type === "CHECK_VORTEX_PENDING_URL") {
      try {
        const resp = await fetch(`${API_BASE}/api/browser-pending`, {
          signal: AbortSignal.timeout(2000),
        });
        if (!resp.ok) {
          sendResponse({ pending: false });
          return;
        }
        const data = await resp.json();
        const norm = normalizeBrowserPageUrl(msg.pageUrl || "");
        const list = Array.isArray(data?.pending_urls) ? data.pending_urls : [];
        const matched = list.some((u) => normalizeBrowserPageUrl(u) === norm);
        sendResponse({ pending: matched });
      } catch (_) {
        sendResponse({ pending: false });
      }
    } else if (msg.type === "BROWSER_RESOLVED_LINK") {
      const pageUrl = String(msg.pageUrl || "");
      const directUrl = msg.directUrl ? String(msg.directUrl) : null;
      const headers = {};
      if (pageUrl) headers.Referer = pageUrl;
      if (msg.userAgent) headers["User-Agent"] = String(msg.userAgent);
      const cookieStr = await collectMergedCookiesHeader(
        directUrl ? [directUrl, pageUrl] : [pageUrl]
      );
      if (cookieStr) headers.Cookie = cookieStr;

      let res = { ok: false, error: "VortexDL offline" };
      try {
        const resp = await fetch(`${API_BASE}/api/browser-resolved`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            page_url: pageUrl,
            direct_url: directUrl,
            filename: msg.filename || null,
            error: msg.error || null,
            headers,
          }),
        });
        if (resp.ok) {
          res = await resp.json();
        } else {
          res = { ok: false, error: `HTTP ${resp.status}` };
        }
      } catch (err) {
        res = { ok: false, error: String(err) };
      }

      const tabId = sender?.tab?.id;
      if (res.ok && tabId) {
        if (res.next_url) {
          setTimeout(() => {
            chrome.tabs.update(tabId, { url: res.next_url, active: true }).catch(() => {});
          }, 450);
        } else if (msg.isAutoResolve) {
          setTimeout(() => {
            chrome.tabs.remove(tabId).catch(() => {});
          }, 1400);
        }
      }
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
