//! Enumeration of the guest's shareable top-level application windows.
//!
//! The filter matters: a naive `EnumWindows` + `IsWindowVisible` pass reports many phantom
//! windows on a real Windows 10/11 desktop (cloaked UWP hosts, tool windows, the shell
//! window, zero-size helpers). Each would otherwise become an empty native Linux window.
//!
//! Geometry is reported as the DWM **extended frame bounds**, not `GetWindowRect`:
//! `GetWindowRect` includes the invisible resize border DWM adds, so it does not match what
//! Windows.Graphics.Capture actually captures. Reporting the frame bounds keeps the client's
//! native window aligned with the pixels, and keeps input coordinates correct.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetShellWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, IsIconic, IsWindowVisible, GWL_EXSTYLE, GWL_STYLE, WS_CHILD, WS_EX_TOOLWINDOW,
};

/// A shareable top-level application window on the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Native window handle as an integer (mapped to a protocol window id by the caller).
    pub hwnd: isize,
    /// Window title.
    pub title: String,
    /// Visible frame bounds in screen coordinates (DWM extended frame bounds).
    pub x: i32,
    /// Visible frame bounds in screen coordinates (DWM extended frame bounds).
    pub y: i32,
    /// Visible frame width.
    pub width: u16,
    /// Visible frame height.
    pub height: u16,
    /// Whether the window is minimized (its geometry is meaningless while it is).
    pub minimized: bool,
}

/// Enumerate visible, titled, non-cloaked top-level application windows.
pub fn enumerate_windows() -> Vec<WindowInfo> {
    // Collect raw handles first; doing the allocating, fallible metadata work outside the
    // callback keeps the FFI callback trivial and panic-free.
    let mut handles: Vec<HWND> = Vec::new();
    // SAFETY: `collect_hwnd` is a valid EnumWindows callback; `lparam` carries a pointer to
    // `handles`, which outlives this synchronous call.
    unsafe {
        let _ = EnumWindows(Some(collect_hwnd), LPARAM(&mut handles as *mut _ as isize));
    }

    handles
        .into_iter()
        .filter(|&hwnd| is_shareable(hwnd))
        .filter_map(describe_window)
        .collect()
}

unsafe extern "system" fn collect_hwnd(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the pointer passed by `enumerate_windows`.
    let handles = &mut *(lparam.0 as *mut Vec<HWND>);
    handles.push(hwnd);
    TRUE
}

/// The standard "is this a real app window the user would see in the taskbar" predicate.
fn is_shareable(hwnd: HWND) -> bool {
    // SAFETY: all calls are simple queries on a handle produced by EnumWindows.
    unsafe {
        if hwnd == GetShellWindow() {
            return false;
        }
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        if GetWindowTextLengthW(hwnd) == 0 {
            return false;
        }

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        if style & WS_CHILD.0 != 0 {
            return false;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }

        // Cloaked windows are the big one: UWP `ApplicationFrameHost` ghosts and windows on
        // another virtual desktop are visible-but-cloaked.
        let mut cloaked: u32 = 0;
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        if hr.is_ok() && cloaked != 0 {
            return false;
        }

        true
    }
}

fn describe_window(hwnd: HWND) -> Option<WindowInfo> {
    // SAFETY: queries on a valid handle; the buffer is sized from the API's own length report.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; len as usize + 1];
        let written = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..written.max(0) as usize]);

        let minimized = IsIconic(hwnd).as_bool();
        let rect = frame_bounds(hwnd)?;
        let width = (rect.right - rect.left).max(0);
        let height = (rect.bottom - rect.top).max(0);
        if !minimized && (width == 0 || height == 0) {
            return None;
        }

        Some(WindowInfo {
            hwnd: hwnd.0 as isize,
            title,
            x: rect.left,
            y: rect.top,
            width: width.min(u16::MAX as i32) as u16,
            height: height.min(u16::MAX as i32) as u16,
            minimized,
        })
    }
}

/// DWM extended frame bounds (what is actually drawn), falling back to `GetWindowRect`.
///
/// # Safety
/// `hwnd` must be a valid window handle.
unsafe fn frame_bounds(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    let hr = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        (&mut rect as *mut RECT).cast(),
        std::mem::size_of::<RECT>() as u32,
    );
    if hr.is_ok() {
        return Some(rect);
    }
    let mut fallback = RECT::default();
    GetWindowRect(hwnd, &mut fallback).ok()?;
    Some(fallback)
}
