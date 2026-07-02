//! Windows-only agent entry point. Currently a pipeline smoke test that references the
//! capture / encode / window-enumeration APIs the agent will use, to prove they compile and
//! link for the Windows target.
#![cfg(windows)]

use windows::Win32::Foundation::HWND;
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// Smoke test: touch the key APIs (window handle, WGC interop type, Media Foundation
/// startup/shutdown) so cross-compilation exercises the real dependency surface.
pub fn run() {
    // SAFETY: standard Win32/MF calls with no invalid arguments.
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        eprintln!("oxagent: foreground window = {hwnd:?}");

        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok() {
            eprintln!("oxagent: Media Foundation started (v{MF_VERSION:#x})");
            let _ = MFShutdown();
        }
    }

    // Reference the WGC interop type so the Graphics_Capture surface is compiled/linked.
    fn _uses_wgc(_: &IGraphicsCaptureItemInterop) {}
    eprintln!("oxagent: WGC interop type resolved");
}
