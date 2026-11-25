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
      document.querySelectorAll("video[src], video source[src], audio[src], audio source[src]").forEach((el) => {
        if (el.src && el.src.startsWith("http")) urls.add(el.src);
      });
    }

    sendResponse({ urls: Array.from(urls) });
  }
  return false;
});
