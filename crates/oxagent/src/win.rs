//! Windows-only agent internals. Currently: enumeration of top-level application windows
//! (the source list for per-window capture). Capture (WGC) and encode (Media Foundation)
//! land next.
#![cfg(windows)]

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

/// A top-level application window discovered on the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Native window handle, as an integer (for logging / mapping to a protocol window id).
    pub hwnd: isize,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

/// Enumerate visible, titled top-level windows.
pub fn enumerate_windows() -> Vec<WindowInfo> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    // SAFETY: `enum_proc` is a valid callback; `lparam` carries a pointer to `windows`,
    // which outlives the synchronous EnumWindows call.
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the pointer to the `Vec<WindowInfo>` passed by `enumerate_windows`.
    let windows = &mut *(lparam.0 as *mut Vec<WindowInfo>);

    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let title_len = GetWindowTextLengthW(hwnd);
    if title_len <= 0 {
        return TRUE;
    }

    let mut buf = vec![0u16; title_len as usize + 1];
    let written = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..written as usize]);

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_ok() {
        windows.push(WindowInfo {
            hwnd: hwnd.0 as isize,
            title,
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left).max(0) as u16,
            height: (rect.bottom - rect.top).max(0) as u16,
        });
    }
    TRUE
}

/// Agent entry point (bring-up): list the windows we can see.
pub fn run() {
    let windows = enumerate_windows();
    eprintln!("oxagent: {} visible top-level window(s)", windows.len());
    for w in windows.iter().take(10) {
        eprintln!(
            "  [{:#x}] {}x{} @({},{})  {}",
            w.hwnd, w.width, w.height, w.x, w.y, w.title
        );
    }
}
