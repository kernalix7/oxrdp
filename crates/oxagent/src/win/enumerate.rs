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

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetShellWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, IsIconic, IsWindowVisible, IsZoomed, GWL_EXSTYLE, GWL_STYLE, WS_CAPTION,
    WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_THICKFRAME,
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
    /// Whether the window is currently maximized.
    pub maximized: bool,
    /// Whether the user can resize the window (`WS_THICKFRAME`).
    pub resizable: bool,
    /// Whether the window has a plain, croppable native caption — see [`has_native_frame`] for
    /// exactly what this does and does not mean.
    pub has_frame: bool,
    /// Whether the window is marked always-on-top (`WS_EX_TOPMOST`).
    pub topmost: bool,
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

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;

        Some(WindowInfo {
            hwnd: hwnd.0 as isize,
            title,
            x: rect.left,
            y: rect.top,
            width: width.min(u16::MAX as i32) as u16,
            height: height.min(u16::MAX as i32) as u16,
            minimized,
            maximized: IsZoomed(hwnd).as_bool(),
            resizable: style & WS_THICKFRAME.0 != 0,
            has_frame: has_native_frame(hwnd, style, &rect),
            topmost: ex_style & WS_EX_TOPMOST.0 != 0,
        })
    }
}

/// A handful of physical pixels of slack for measurement rounding in [`has_native_frame`]. Any
/// genuine system caption is tens of pixels tall even at the smallest supported scale factor,
/// so this cannot misclassify a real one — it only correctly rejects a near-zero gap.
const FRAME_EXTENSION_TOLERANCE_PX: i32 = 8;

/// Whether `hwnd` has a plain, native Windows caption that is safe to treat as croppable chrome
/// — the meaning `WindowOpened.flags`' `HAS_FRAME` bit carries on the wire (`OXPROTO.md` §11):
/// **true** means the client may crop the captured pixels down to the client area and wrap them
/// in its own native decoration; **false** means it must render the captured frame border-to-
/// border, exactly as captured, because there is either no frame at all (a tooltip, a splash
/// screen) or a frame whose content is not plain system chrome.
///
/// `WS_CAPTION` alone is not sufficient. Windows Terminal (and other apps that call
/// `DwmExtendFrameIntoClientArea` to draw a custom tab strip / search box in the caption band)
/// keep `WS_CAPTION` set purely to retain Aero Snap and the system menu, while the pixels in
/// that band are the app's own UI, not a generic title bar — cropping them would slice off real
/// content, not chrome. The reliable signal is *where the client area actually starts*: for a
/// plain captioned window it begins a caption's height below the frame's top edge (in screen
/// coordinates); for a frame-extended window it begins right at — or within a few pixels of —
/// the frame's own top, because the app has claimed that space for itself. Comparing
/// `ClientToScreen` against the same extended frame bounds `frame_bounds` already computes (so
/// this needs no extra DWM query) distinguishes the two without knowing anything
/// app-specific.
fn has_native_frame(hwnd: HWND, style: u32, frame: &RECT) -> bool {
    if style & WS_CAPTION.0 == 0 {
        return false;
    }
    let mut origin = POINT::default();
    // SAFETY: `hwnd` is the same handle `describe_window` is already querying; `origin` is a
    // valid, uniquely-owned out-parameter for the duration of this call.
    if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
        // Cannot tell: assume no croppable frame rather than guessing wrong and cropping into
        // real content.
        return false;
    }
    origin.y.saturating_sub(frame.top) >= FRAME_EXTENSION_TOLERANCE_PX
}

/// DWM extended frame bounds (what is actually drawn), falling back to `GetWindowRect`.
///
/// `pub(crate)` because [`crate::win::input`] needs the same rectangle to convert a wire
/// `PointerEvent`'s window-relative coordinates to guest screen coordinates — it must be
/// exactly this rectangle, not `GetWindowRect`, or clicks drift from what the user sees the
/// captured frame land on.
///
/// **Coupling to watch when `HAS_FRAME`-based cropping lands** (`docs/design/
/// window-decorations.md`): this function reports the *whole* window today, and capture
/// (`crate::win::capture`) still sends the whole window's pixels, so "whole window" is
/// consistently the coordinate space everywhere — reported geometry, captured frames, and
/// `PointerEvent`'s window-relative origin all agree. The moment the agent starts cropping a
/// `HAS_FRAME` window's frame down to its client area, this function (and every geometry field
/// derived from it) must switch to the client rect for that window too, or reported geometry
/// stops matching the pixels; and `crate::win::input`'s pointer-coordinate conversion, which
/// currently anchors on this exact rectangle, must move with it or every click on a cropped
/// window lands off by the caption height. Populating `HAS_FRAME` does not do any of that
/// itself — it only reports the fact; acting on it in capture/geometry/input is future work,
/// and all three have to move together when it happens.
///
/// # Safety
/// `hwnd` must be a valid window handle.
pub(crate) unsafe fn frame_bounds(hwnd: HWND) -> Option<RECT> {
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
