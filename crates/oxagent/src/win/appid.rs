//! Window -> application identity resolution.
//!
//! The client uses [`AppIdentity::app_id`] as the Linux-side `WM_CLASS`, so every window
//! belonging to the same executable must resolve to the same id, and windows from different
//! executables must not collide into one. Getting this wrong is the whole failure mode the
//! project exists to avoid: without it, every guest window looks like the same generic
//! "windows-app" on the Linux desktop.
//!
//! Resolution costs two syscalls beyond the pid lookup that already runs on every window on
//! every tick: opening a handle to the process, then asking that process for its own image
//! path. Both are cheap individually but wasteful to repeat 60 times a second for a window
//! whose owning process cannot have changed, so results are cached and only recomputed when
//! the observed pid for a given `HWND` changes — which also happens to be exactly the
//! condition under which the cached answer would otherwise go stale (Windows recycles `HWND`
//! values once the original window is destroyed, so a later, unrelated window can reuse one).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE, HWND};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// The application a window belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// Owning process id.
    pub pid: u32,
    /// Executable base name, lowercased (e.g. `notepad.exe`) — becomes the client's WM_CLASS.
    pub app_id: String,
}

/// A cached resolution: the pid observed when the row was written, and the outcome for that
/// pid. The outcome is cached as `None` too — a window owned by a more privileged process
/// stays that way for the life of the process, so remembering the failure avoids retrying
/// `OpenProcess` against it on every tick.
struct CacheEntry {
    pid: u32,
    identity: Option<AppIdentity>,
}

/// `HWND -> CacheEntry`, behind a mutex because [`identify`] runs once per window on every
/// capture tick and may be called from whichever worker thread the async runtime schedules the
/// session task onto.
///
/// A free-standing cache (rather than a struct threaded through the caller) keeps `identify`
/// usable as a plain function with the same signature regardless of who calls it or how many
/// [`crate::win::WinWindowSource`] instances exist; process identity is a global fact about the
/// guest OS, not per-session state, so there is nothing session-scoped to model here.
fn cache() -> &'static Mutex<HashMap<isize, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<isize, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the application owning `hwnd`. Returns `None` if the process cannot be opened,
/// which is normal for windows owned by a more privileged process.
pub fn identify(hwnd: HWND) -> Option<AppIdentity> {
    let pid = window_pid(hwnd)?;
    let key = hwnd.0 as isize;

    let mut cache = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = cache.get(&key) {
        if entry.pid == pid {
            return entry.identity.clone();
        }
    }

    let identity = resolve(pid);
    cache.insert(
        key,
        CacheEntry {
            pid,
            identity: identity.clone(),
        },
    );
    identity
}

/// The process id that currently owns `hwnd`, or `None` if the handle is no longer a valid
/// window (it can go stale between enumeration and this call, e.g. the window just closed).
fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: `hwnd` is a window handle the caller obtained from window enumeration; `pid` is
    // a valid, uniquely-owned out-pointer for the duration of this call.
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // GetWindowThreadProcessId returns 0 (and does not write `pid`) for an invalid handle.
    (thread_id != 0 && pid != 0).then_some(pid)
}

/// Open `pid`, ask it for its own image path, and reduce that to a lowercased executable file
/// name. Fails whenever the process cannot be opened at all — normal for system/elevated
/// processes a non-elevated agent has no rights to.
fn resolve(pid: u32) -> Option<AppIdentity> {
    // SAFETY: `PROCESS_QUERY_LIMITED_INFORMATION` is the minimal access right that lets a
    // non-elevated caller query a process's image name (unlike `PROCESS_QUERY_INFORMATION`,
    // which most other processes will deny); `FALSE` means the returned handle is not
    // inherited by child processes. `pid` came from `GetWindowThreadProcessId` moments ago, so
    // it names a real process, though it may have already exited.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) }.ok()?;
    // Guard immediately: every return below (including `?`) must still close the handle.
    let handle = ProcessHandleGuard(handle);

    let path = query_image_name(handle.0)?;
    let app_id = Path::new(&path).file_name()?.to_str()?.to_ascii_lowercase();

    Some(AppIdentity { pid, app_id })
}

/// The full image path of the process behind `handle`, or `None` if the query fails.
fn query_image_name(handle: HANDLE) -> Option<String> {
    // Long enough for any realistically installed executable path. `len` carries the buffer's
    // capacity in and the actual length written back out, so an undersized buffer would fail
    // the call cleanly rather than truncate the path silently.
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` was opened with query rights by the caller and is still open; `buf` is
    // a stack buffer of `buf.len()` u16s and `len` starts at that same capacity, matching what
    // `QueryFullProcessImageNameW` requires of its buffer/size pair.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Closes the wrapped process handle on drop, so every return path out of [`resolve`] —
/// including the early returns from `?` — still releases it.
struct ProcessHandleGuard(HANDLE);

impl Drop for ProcessHandleGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned by a successful `OpenProcess` call in `resolve`, is
        // owned solely by this guard, and is not closed anywhere else.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
