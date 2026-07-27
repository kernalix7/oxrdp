//! Windows-only agent internals: window enumeration and per-window capture.
#![cfg(windows)]

pub mod capture;
pub mod enumerate;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use capture::WindowCapture;
use enumerate::enumerate_windows;

/// Opt into per-monitor DPI awareness.
///
/// Must happen before any window geometry is read: without it Windows lies to the process
/// about window rectangles on a scaled display (returning virtualized, scaled-down
/// coordinates), which would misalign every captured window and every input coordinate.
fn set_dpi_awareness() {
    // SAFETY: no arguments beyond a well-known context constant; failure is non-fatal.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Agent entry point (bring-up): report capture support, list windows, and capture one frame
/// from the first window to prove the pipeline end to end on the guest.
pub fn run() {
    set_dpi_awareness();

    eprintln!(
        "oxagent: Windows.Graphics.Capture supported = {}",
        capture::is_supported()
    );

    let windows = enumerate_windows();
    eprintln!("oxagent: {} shareable window(s)", windows.len());
    for w in windows.iter().take(10) {
        eprintln!(
            "  [{:#x}] {}x{} @({},{}){}  {}",
            w.hwnd,
            w.width,
            w.height,
            w.x,
            w.y,
            if w.minimized { " [min]" } else { "" },
            w.title
        );
    }

    let Some(target) = windows.iter().find(|w| !w.minimized) else {
        eprintln!("oxagent: no capturable window found");
        return;
    };
    eprintln!("oxagent: capturing '{}' ...", target.title);

    let hwnd = HWND(target.hwnd as *mut core::ffi::c_void);
    match WindowCapture::new(hwnd) {
        Ok(mut cap) => {
            if let Ok(size) = cap.size() {
                eprintln!("oxagent: capture item size {}x{}", size.Width, size.Height);
            }
            // WGC delivers asynchronously; poll briefly for the first frame.
            for _ in 0..120 {
                match cap.try_next_frame() {
                    Ok(Some(frame)) => {
                        eprintln!(
                            "oxagent: captured frame {}x{} ({} bytes BGRA)",
                            frame.width,
                            frame.height,
                            frame.bgra.len()
                        );
                        return;
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(16)),
                    Err(e) => {
                        eprintln!("oxagent: capture error: {e}");
                        return;
                    }
                }
            }
            eprintln!("oxagent: no frame arrived within ~2s");
        }
        Err(e) => eprintln!("oxagent: failed to start capture: {e}"),
    }
}
