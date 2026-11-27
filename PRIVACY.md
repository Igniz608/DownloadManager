# Privacy Policy — VortexDL Browser Extension

**Last updated:** November 27, 2026

The **VortexDL — Smart Download & Media Capture** browser extension (for Microsoft Edge and compatible browsers) is designed to operate **100% locally** on your device as a companion to the **VortexDL** desktop application.

## 1. Data We Access and Process Locally

To fulfill its single purpose of capturing downloads, detecting streaming media, and extracting user-selected links for VortexDL, the extension accesses the following data **only on your local device**:

- **Authentication Information (Session Cookies & Request Headers):** When a download or media stream is handed off to the local VortexDL desktop application, the extension reads relevant session cookies (`chrome.cookies`) and HTTP request headers (`Referer`, `Origin`, `User-Agent`) for that specific target URL so authenticated downloads can complete in VortexDL without requiring re-login.
- **User Activity (Network Request Metadata):** The extension passively observes network request/response headers (`webRequest`) to identify downloadable media streams (`.m3u8`, `.mpd`, `.mp4`, etc.) in active browser tabs.
- **Website Content (Links & Media URLs):** When you trigger "Send page links to LinkGrabber", "Resolve links", or click the video download overlay, the extension inspects hyperlinks and media element URLs in the active tab to present them for download.
- **Local Extension Settings:** User preferences (such as auto-interception rules, minimum file size, and overlay position) are stored locally in browser extension storage (`chrome.storage.local` and `chrome.storage.session`).

## 2. How Data Is Transferred and Stored

- **No External Servers or Telemetry:** The extension does **not** transmit, upload, sell, or share any browsing data, cookies, URLs, or personal information to the developer, analytics providers, or any third-party servers.
- **Local-Only Communication:** Captured download URLs, headers, cookies, and links are sent exclusively to the **VortexDL** desktop application running on your own computer via `http://127.0.0.1:17654` and the local Native Messaging host (`com.vortexdl.host`).

## 3. Data Usage Certifications

- We do **not** sell or transfer user data to third parties.
- We do **not** use or transfer user data for purposes unrelated to the extension's single purpose.
- We do **not** use or transfer user data to determine creditworthiness or for lending purposes.

## 4. User Control & Removal

You can disable download interception, video overlays, or specific capture behaviors at any time in the extension popup, or revoke all access immediately by disabling or removing the extension from Microsoft Edge (`edge://extensions`).

## 5. Contact

Questions or privacy inquiries can be submitted via the project repository:
https://github.com/Igniz608/DownloadManager/issues
