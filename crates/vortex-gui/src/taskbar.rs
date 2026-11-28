use vortex_core::model::{DownloadItem, DownloadStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarProgress {
    None,
    Indeterminate,
    Normal(u16),
    Paused(u16),
}

pub fn compute_taskbar_progress(downloads: &[DownloadItem]) -> TaskbarProgress {
    let mut active_count = 0usize;
    let mut active_known_count = 0usize;
    let mut active_sum = 0.0f64;

    let mut paused_count = 0usize;
    let mut paused_sum = 0.0f64;

    for item in downloads {
        match item.status {
            DownloadStatus::Queued
            | DownloadStatus::Probing
            | DownloadStatus::Downloading
            | DownloadStatus::Merging
            | DownloadStatus::Extracting => {
                active_count += 1;
                let (known, frac) = match item.status {
                    DownloadStatus::Extracting => match item.extraction_progress {
                        Some(p) => (true, p.clamp(0.0, 1.0) as f64),
                        None => (false, 0.0),
                    },
                    DownloadStatus::Merging => match item.total_bytes {
                        Some(t) if t > 0 => (true, item.progress_fraction() as f64),
                        _ => (true, 1.0),
                    },
                    _ => match item.total_bytes {
                        Some(t) if t > 0 => (true, item.progress_fraction() as f64),
                        _ => (false, 0.0),
                    },
                };
                if known {
                    active_known_count += 1;
                }
                active_sum += frac.clamp(0.0, 1.0);
            }
            DownloadStatus::Paused => {
                paused_count += 1;
                paused_sum += (item.progress_fraction() as f64).clamp(0.0, 1.0);
            }
            DownloadStatus::Seeding | DownloadStatus::Completed | DownloadStatus::Failed(_) => {}
        }
    }

    if active_count > 0 {
        if active_known_count == 0 {
            return TaskbarProgress::Indeterminate;
        }
        let avg = (active_sum / active_count as f64).clamp(0.0, 1.0);
        let permille = (avg * 1000.0).round().clamp(0.0, 1000.0) as u16;
        return TaskbarProgress::Normal(permille);
    }

    if paused_count > 0 {
        let avg = (paused_sum / paused_count as f64).clamp(0.0, 1.0);
        let permille = (avg * 1000.0).round().clamp(0.0, 1000.0) as u16;
        return TaskbarProgress::Paused(permille);
    }

    TaskbarProgress::None
}

#[cfg(windows)]
pub struct TaskbarProgressController {
    taskbar: *mut ITaskbarList3,
    hwnd: isize,
    last_sent: Option<TaskbarProgress>,
    com_initialized: bool,
}

#[cfg(windows)]
impl TaskbarProgressController {
    pub fn new() -> Self {
        Self {
            taskbar: std::ptr::null_mut(),
            hwnd: 0,
            last_sent: None,
            com_initialized: false,
        }
    }

    pub fn update(&mut self, progress: TaskbarProgress) {
        if self.last_sent == Some(progress) {
            return;
        }
        if !self.ensure_hwnd() || !self.ensure_taskbar() {
            return;
        }

        let ok = unsafe {
            let vtbl = &*(*self.taskbar).vtbl;
            match progress {
                TaskbarProgress::None => (vtbl.set_progress_state)(self.taskbar, self.hwnd, TBPF_NOPROGRESS) >= 0,
                TaskbarProgress::Indeterminate => {
                    (vtbl.set_progress_state)(self.taskbar, self.hwnd, TBPF_INDETERMINATE) >= 0
                }
                TaskbarProgress::Normal(val) => {
                    let s = (vtbl.set_progress_state)(self.taskbar, self.hwnd, TBPF_NORMAL);
                    let v = (vtbl.set_progress_value)(self.taskbar, self.hwnd, val as u64, 1000);
                    s >= 0 && v >= 0
                }
                TaskbarProgress::Paused(val) => {
                    // Per Win32 ITaskbarList3 docs, SetProgressValue must precede SetProgressState(TBPF_PAUSED).
                    let v = (vtbl.set_progress_value)(self.taskbar, self.hwnd, val as u64, 1000);
                    let s = (vtbl.set_progress_state)(self.taskbar, self.hwnd, TBPF_PAUSED);
                    v >= 0 && s >= 0
                }
            }
        };

        if ok {
            self.last_sent = Some(progress);
        } else {
            self.hwnd = 0;
        }
    }

    fn ensure_hwnd(&mut self) -> bool {
        if self.hwnd != 0 && unsafe { IsWindow(self.hwnd) != 0 } {
            return true;
        }
        self.hwnd = find_main_window_hwnd();
        self.hwnd != 0
    }

    fn ensure_taskbar(&mut self) -> bool {
        if !self.taskbar.is_null() {
            return true;
        }
        unsafe {
            if !self.com_initialized {
                let hr = CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
                // S_OK (0) or S_FALSE (1) means we should balance with CoUninitialize; RPC_E_CHANGED_MODE means already initialized.
                self.com_initialized = hr == 0 || hr == 1;
            }
            let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_TASKBAR_LIST,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASKBAR_LIST3,
                &mut ptr,
            );
            if hr < 0 || ptr.is_null() {
                return false;
            }
            let tb = ptr as *mut ITaskbarList3;
            let hr_init = ((*(*tb).vtbl).hr_init)(tb);
            if hr_init < 0 {
                ((*(*tb).vtbl).release)(tb);
                return false;
            }
            self.taskbar = tb;
            true
        }
    }
}

#[cfg(windows)]
impl Drop for TaskbarProgressController {
    fn drop(&mut self) {
        unsafe {
            if !self.taskbar.is_null() {
                ((*(*self.taskbar).vtbl).release)(self.taskbar);
                self.taskbar = std::ptr::null_mut();
            }
            if self.com_initialized {
                CoUninitialize();
                self.com_initialized = false;
            }
        }
    }
}

#[cfg(not(windows))]
pub struct TaskbarProgressController;

#[cfg(not(windows))]
impl TaskbarProgressController {
    pub fn new() -> Self {
        Self
    }

    pub fn update(&mut self, _progress: TaskbarProgress) {}
}

#[cfg(windows)]
const TBPF_NOPROGRESS: u32 = 0x0;
#[cfg(windows)]
const TBPF_INDETERMINATE: u32 = 0x1;
#[cfg(windows)]
const TBPF_NORMAL: u32 = 0x2;
#[cfg(windows)]
const TBPF_PAUSED: u32 = 0x8;
#[cfg(windows)]
const COINIT_APARTMENTTHREADED: u32 = 0x2;
#[cfg(windows)]
const CLSCTX_INPROC_SERVER: u32 = 0x1;

#[cfg(windows)]
#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[cfg(windows)]
const CLSID_TASKBAR_LIST: Guid = Guid {
    data1: 0x56fdf344,
    data2: 0xfd6d,
    data3: 0x11d0,
    data4: [0x95, 0x8a, 0x00, 0x60, 0x97, 0xc9, 0xa0, 0x90],
};

#[cfg(windows)]
const IID_ITASKBAR_LIST3: Guid = Guid {
    data1: 0xea1afb91,
    data2: 0x9e28,
    data3: 0x4b86,
    data4: [0x90, 0xe9, 0x9e, 0x9f, 0x8a, 0x5e, 0xef, 0xaf],
};

#[cfg(windows)]
#[repr(C)]
struct ITaskbarList3 {
    vtbl: *const ITaskbarList3Vtbl,
}

#[cfg(windows)]
#[repr(C)]
struct ITaskbarList3Vtbl {
    query_interface: unsafe extern "system" fn(
        *mut ITaskbarList3,
        *const Guid,
        *mut *mut std::ffi::c_void,
    ) -> i32,
    add_ref: unsafe extern "system" fn(*mut ITaskbarList3) -> u32,
    release: unsafe extern "system" fn(*mut ITaskbarList3) -> u32,
    hr_init: unsafe extern "system" fn(*mut ITaskbarList3) -> i32,
    add_tab: unsafe extern "system" fn(*mut ITaskbarList3, isize) -> i32,
    delete_tab: unsafe extern "system" fn(*mut ITaskbarList3, isize) -> i32,
    activate_tab: unsafe extern "system" fn(*mut ITaskbarList3, isize) -> i32,
    set_active_alt: unsafe extern "system" fn(*mut ITaskbarList3, isize) -> i32,
    mark_fullscreen_window: unsafe extern "system" fn(*mut ITaskbarList3, isize, i32) -> i32,
    set_progress_value: unsafe extern "system" fn(*mut ITaskbarList3, isize, u64, u64) -> i32,
    set_progress_state: unsafe extern "system" fn(*mut ITaskbarList3, isize, u32) -> i32,
}

#[cfg(windows)]
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(pvReserved: *mut std::ffi::c_void, dwCoInit: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        rclsid: *const Guid,
        pUnkOuter: *mut std::ffi::c_void,
        dwClsContext: u32,
        riid: *const Guid,
        ppv: *mut *mut std::ffi::c_void,
    ) -> i32;
}

#[cfg(windows)]
unsafe extern "system" {
    fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> isize;
    fn IsWindow(hWnd: isize) -> i32;
    fn EnumWindows(
        lpEnumFunc: unsafe extern "system" fn(isize, isize) -> i32,
        lParam: isize,
    ) -> i32;
    fn GetWindowThreadProcessId(hWnd: isize, lpdwProcessId: *mut u32) -> u32;
    fn GetCurrentProcessId() -> u32;
    fn GetWindow(hWnd: isize, uCmd: u32) -> isize;
    fn GetWindowTextLengthW(hWnd: isize) -> i32;
    fn GetWindowLongW(hWnd: isize, nIndex: i32) -> i32;
}

#[cfg(windows)]
fn find_main_window_hwnd() -> isize {
    let title: Vec<u16> = "VortexDL — Next-Gen Rust Download Manager"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let by_title = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if by_title != 0 {
        return by_title;
    }

    #[repr(C)]
    struct EnumCtx {
        pid: u32,
        found: isize,
    }

    unsafe extern "system" fn enum_proc(hwnd: isize, lparam: isize) -> i32 {
        const GW_OWNER: u32 = 4;
        const GWL_STYLE: i32 = -16;
        const WS_CAPTION: i32 = 0x00C0_0000;
        unsafe {
            let ctx = &mut *(lparam as *mut EnumCtx);
            let mut win_pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut win_pid);
            if win_pid == ctx.pid
                && GetWindow(hwnd, GW_OWNER) == 0
                && GetWindowTextLengthW(hwnd) > 0
                && (GetWindowLongW(hwnd, GWL_STYLE) & WS_CAPTION) != 0
            {
                ctx.found = hwnd;
                return 0;
            }
            1
        }
    }

    let mut ctx = EnumCtx {
        pid: unsafe { GetCurrentProcessId() },
        found: 0,
    };
    unsafe {
        let _ = EnumWindows(enum_proc, (&mut ctx as *mut EnumCtx) as isize);
    }
    ctx.found
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use uuid::Uuid;
    use vortex_core::model::{DownloadKind, FileCategory};

    fn make_item(
        status: DownloadStatus,
        downloaded: u64,
        total: Option<u64>,
        extraction_progress: Option<f32>,
    ) -> DownloadItem {
        DownloadItem {
            id: Uuid::new_v4(),
            package_id: None,
            part_index: None,
            part_total: None,
            kind: DownloadKind::Http,
            status,
            url: "https://example.com/file.bin".into(),
            filename: "file.bin".into(),
            save_dir: PathBuf::from("."),
            file_path: PathBuf::from("file.bin"),
            category: FileCategory::Other,
            total_bytes: total,
            downloaded_bytes: downloaded,
            uploaded_bytes: 0,
            speed_bps: 0,
            upload_speed_bps: 0,
            eta_seconds: None,
            connections: 1,
            supports_ranges: true,
            segments: Vec::new(),
            etag: None,
            last_modified: None,
            headers: HashMap::new(),
            checksum_expected: None,
            checksum_verified: None,
            media_info: None,
            torrent_info: None,
            auto_extract: false,
            extractor_override: None,
            extract_password: None,
            extracted_to: None,
            extraction_status: None,
            extraction_progress,
            created_at: Utc::now(),
            completed_at: None,
            error: None,
            priority: 0,
        }
    }

    #[test]
    fn single_and_multiple_active_downloads_use_arithmetic_mean() {
        let one = vec![make_item(DownloadStatus::Downloading, 250, Some(1000), None)];
        assert_eq!(compute_taskbar_progress(&one), TaskbarProgress::Normal(250));

        // 80% + 20% => 50% average regardless of total file sizes.
        let two = vec![
            make_item(DownloadStatus::Downloading, 8_000_000, Some(10_000_000), None),
            make_item(DownloadStatus::Downloading, 200, Some(1000), None),
            make_item(DownloadStatus::Completed, 1000, Some(1000), None),
        ];
        assert_eq!(compute_taskbar_progress(&two), TaskbarProgress::Normal(500));

        // 1 downloading at 60% + 1 queued at 0% => 30% average.
        let with_queued = vec![
            make_item(DownloadStatus::Downloading, 60, Some(100), None),
            make_item(DownloadStatus::Queued, 0, Some(100), None),
        ];
        assert_eq!(
            compute_taskbar_progress(&with_queued),
            TaskbarProgress::Normal(300)
        );
    }

    #[test]
    fn handles_indeterminate_paused_extracting_and_idle() {
        let unknown = vec![make_item(DownloadStatus::Probing, 0, None, None)];
        assert_eq!(
            compute_taskbar_progress(&unknown),
            TaskbarProgress::Indeterminate
        );

        let paused = vec![
            make_item(DownloadStatus::Paused, 40, Some(100), None),
            make_item(DownloadStatus::Paused, 80, Some(100), None),
        ];
        assert_eq!(
            compute_taskbar_progress(&paused),
            TaskbarProgress::Paused(600)
        );

        let extracting = vec![make_item(DownloadStatus::Extracting, 100, Some(100), Some(0.75))];
        assert_eq!(
            compute_taskbar_progress(&extracting),
            TaskbarProgress::Normal(750)
        );

        let finished_and_seeding = vec![
            make_item(DownloadStatus::Completed, 100, Some(100), None),
            make_item(DownloadStatus::Seeding, 100, Some(100), None),
        ];
        assert_eq!(
            compute_taskbar_progress(&finished_and_seeding),
            TaskbarProgress::None
        );
    }
}
