function formatBytes(bytes) {
  if (!bytes) return "";
  const units = ["B", "KB", "MB", "GB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

function setFeedback(text, isError = false) {
  const el = document.getElementById("feedback");
  el.textContent = text;
  el.style.color = isError ? "#ef4d60" : "#3cd070";
}

async function init() {
  const statusBadge = document.getElementById("statusBadge");
  const interceptToggle = document.getElementById("interceptToggle");
  const videoOverlayToggle = document.getElementById("videoOverlayToggle");
  const overlayHoverToggle = document.getElementById("overlayHoverToggle");
  const mediaList = document.getElementById("mediaList");
  const mediaCount = document.getElementById("mediaCount");

  const stored = await chrome.storage.local.get("vortexSettings");
  const currentSettings = stored.vortexSettings || {
    interceptDownloads: true,
    showVideoOverlay: true,
    overlayOnlyOnHover: false,
  };
  interceptToggle.checked = currentSettings.interceptDownloads !== false;
  videoOverlayToggle.checked = currentSettings.showVideoOverlay !== false;
  overlayHoverToggle.checked = Boolean(currentSettings.overlayOnlyOnHover);
  overlayHoverToggle.disabled = !videoOverlayToggle.checked;

  interceptToggle.addEventListener("change", async () => {
    currentSettings.interceptDownloads = interceptToggle.checked;
    await chrome.storage.local.set({ vortexSettings: currentSettings });
    setFeedback(interceptToggle.checked ? "Interceptação ativada." : "Interceptação pausada.");
  });

  videoOverlayToggle.addEventListener("change", async () => {
    currentSettings.showVideoOverlay = videoOverlayToggle.checked;
    overlayHoverToggle.disabled = !videoOverlayToggle.checked;
    await chrome.storage.local.set({ vortexSettings: currentSettings });
    setFeedback(
      videoOverlayToggle.checked
        ? "Botão sobre vídeos ativado."
        : "Botão sobre vídeos ocultado."
    );
  });

  overlayHoverToggle.addEventListener("change", async () => {
    currentSettings.overlayOnlyOnHover = overlayHoverToggle.checked;
    await chrome.storage.local.set({ vortexSettings: currentSettings });
    setFeedback(
      overlayHoverToggle.checked
        ? "Botão exibido somente ao passar o mouse no vídeo."
        : "Botão sempre visível sobre vídeos."
    );
  });

  chrome.runtime.sendMessage({ type: "CHECK_STATUS" }, (res) => {
    if (res?.online) {
      statusBadge.textContent = `Conectado (${res.data?.active_downloads || 0} ativos)`;
      statusBadge.className = "badge online";
    } else {
      statusBadge.textContent = "Offline (Native Host abre ao baixar)";
      statusBadge.className = "badge offline";
    }
  });

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return;

  document.getElementById("btnDownloadPageVideo").addEventListener("click", () => {
    if (!tab.url) return;
    chrome.runtime.sendMessage(
      {
        type: "SEND_MEDIA",
        url: tab.url,
        title: tab.title,
        headers: { Referer: tab.url },
      },
      (res) => {
        if (res?.ok !== false) {
          setFeedback("Página enviada para o downloader de vídeo!");
        } else {
          setFeedback(`Erro: ${res?.error || "Verifique o VortexDL"}`, true);
        }
      }
    );
  });

  document.getElementById("btnGrabAllLinks").addEventListener("click", () => {
    setFeedback("Coletando links da página...");
    chrome.runtime.sendMessage(
      { type: "COLLECT_TAB_LINKS", tabId: tab.id, selectionOnly: false },
      (collected) => {
        const urls = collected?.urls || [];
        if (urls.length === 0) {
          if (collected?.error && tab.url?.startsWith("file://")) {
            setFeedback("Ative 'Permitir acesso a URLs de arquivo' nos detalhes da extensão.", true);
          } else if (collected?.hasDownloadButtons) {
            setFeedback("Botões dinâmicos (JS) — clique em DOWNLOAD na página para interceptar.", true);
          } else {
            setFeedback("Nenhum link encontrado na página.", true);
          }
          return;
        }
        chrome.runtime.sendMessage(
          { type: "SEND_BATCH_LINKS", urls, pageUrl: tab.url },
          (apiRes) => {
            if (apiRes?.ok !== false) {
              setFeedback(`${urls.length} link(s) enviados para o LinkGrabber!`);
            } else {
              setFeedback(`Erro: ${apiRes?.error || "Falha ao enviar"}`, true);
            }
          }
        );
      }
    );
  });

  chrome.runtime.sendMessage({ type: "GET_TAB_MEDIA", tabId: tab.id }, (res) => {
    const items = res?.items || [];
    mediaCount.textContent = String(items.length);
    if (items.length === 0) return;

    mediaList.innerHTML = "";
    for (const item of items) {
      const row = document.createElement("div");
      row.className = "media-item";

      const meta = document.createElement("div");
      meta.className = "media-meta";

      const urlEl = document.createElement("div");
      urlEl.className = "media-url";
      urlEl.title = item.url;
      urlEl.textContent = item.url.split("/").pop()?.split("?")[0] || item.url;

      const subEl = document.createElement("div");
      subEl.className = "media-sub";
      const tag = item.isHls ? "HLS .m3u8" : item.isDash ? "DASH .mpd" : item.contentType || "Mídia";
      const size = formatBytes(item.sizeBytes);
      subEl.textContent = [tag, size].filter(Boolean).join(" • ");

      meta.appendChild(urlEl);
      meta.appendChild(subEl);

      const btn = document.createElement("button");
      btn.className = "mini-btn";
      btn.textContent = "Baixar";
      btn.addEventListener("click", () => {
        const msgType = item.isHls || item.isDash ? "SEND_MEDIA" : "SEND_DOWNLOAD";
        chrome.runtime.sendMessage(
          {
            type: msgType,
            url: item.url,
            title: tab.title,
            headers: { ...(item.headers || {}), Referer: tab.url },
            isHls: item.isHls,
            isDash: item.isDash,
          },
          (r) => {
            if (r?.ok !== false) {
              setFeedback("Stream enviado ao VortexDL!");
            } else {
              setFeedback("Erro ao enviar stream.", true);
            }
          }
        );
      });

      row.appendChild(meta);
      row.appendChild(btn);
      mediaList.appendChild(row);
    }
  });
}

document.addEventListener("DOMContentLoaded", init);
