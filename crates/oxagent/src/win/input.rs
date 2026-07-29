//! Windows implementation of [`InputSink`] via `SendInput` (`docs/design/OXPROTO.md` §13).
//!
//! Deliberate choices:
//! - **Keyboard by scancode, never by virtual key.** `KeyEvent` carries a PS/2 set-1 scancode
//!   straight from the client's xkb keymap; `KEYEVENTF_SCANCODE` injects it unchanged, so the
//!   guest applies its own layout. Driving this guest through synthetic X11 key events
//!   (`xdotool type`) instead mangled everything — Shift was never transmitted, so `C:` arrived
//!   as `c;` and all case was lost. Scancode injection plus the Unicode path below is what
//!   avoids that class of bug entirely; it never depends on either side's layout.
//! - **`TextInput` by `KEYEVENTF_UNICODE`.** For IME/emoji text with no scancode, each UTF-16
//!   code unit (including each half of a surrogate pair) is injected directly as a Unicode
//!   character event, independent of the guest keyboard layout.
//! - **Pointer coordinates are converted, not passed through.** The wire sends window-relative
//!   coordinates (OXPROTO.md §13); `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` requires
//!   normalized 0..65535 coordinates over the *entire* virtual desktop, whose origin is not
//!   necessarily `(0, 0)` — a monitor to the left of or above the primary reports negative
//!   `SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`. Both are read fresh on every event since a
//!   monitor can be hot-plugged mid-session.
//!
//!   The window-relative origin this conversion anchors on is [`enumerate::frame_bounds`] — the
//!   *whole* window, matching what capture sends today. If `HAS_FRAME`-based cropping lands
//!   (`docs/design/window-decorations.md`), a cropped window's reported geometry becomes its
//!   client rect, not its frame rect, and this conversion has to switch origins with it or
//!   every click on a cropped window lands off by the caption height — see the coupling note on
//!   `frame_bounds` itself.

use core::ffi::c_void;

use oxproto::message::input::{lock_state, modifier, pointer_button, window_action};
use oxproto::scancode::Key;
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE,
    KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_CAPITAL, VK_LCONTROL, VK_LMENU,
    VK_LSHIFT, VK_LWIN, VK_NUMLOCK, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PostMessageW, SetForegroundWindow, SetWindowPos, ShowWindow,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WM_CLOSE, XBUTTON1,
    XBUTTON2,
};

use super::enumerate;
use crate::input::InputSink;

/// Injects input on the Windows guest via `SendInput`.
///
/// # Focus
/// `SendInput` has no "inject into this HWND" primitive: injected keyboard and mouse-button
/// input always goes to whatever window currently has Win32 focus. So a session driving one
/// particular window must bring it to the foreground before injecting into it — but doing that
/// on *every* event would fight a user physically at the guest console and flicker the
/// z-order. This struct foregrounds a window only on a state change:
/// - the first pointer button-press edge that targets a window other than the one last
///   focused ([`WinInputSink::pointer_event`]), so passive mouse movement over a window never
///   steals focus, only an actual click does;
/// - an explicit `WindowControl::ACTIVATE`, which *always* forces it, unconditionally on the
///   `last_focused` bookkeeping — it is itself the client's explicit "bring this forward"
///   request (e.g. the user clicked a taskbar entry for a minimized window), and skipping it
///   just because this struct's state still points at that same handle from before the window
///   was minimized would silently drop the request.
#[derive(Debug, Default)]
pub struct WinInputSink {
    /// Handle last given an explicit `SetForegroundWindow`, so repeated clicks inside the
    /// window that already has focus do not re-issue it.
    last_focused: Option<isize>,
    /// Buttons reported held by the previous `PointerEvent`, so the next one's absolute
    /// bitmask can be turned into the down/up transitions `SendInput` wants.
    last_buttons: u8,
}

impl WinInputSink {
    /// A sink with no window focused and no buttons held.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bring `handle` (as `hwnd`) to the foreground if it is not already the tracked focus.
    fn focus_on_change(&mut self, hwnd: HWND, handle: isize) {
        if self.last_focused != Some(handle) {
            set_foreground(hwnd, handle);
            self.last_focused = Some(handle);
        }
    }
}

/// Bring `hwnd` to the foreground and log whether it worked.
///
/// `SetForegroundWindow`'s `BOOL` return used to be discarded outright with `let _ =`. That
/// matters more than most discarded booleans here: Windows imposes documented conditions on
/// when a background process may steal the foreground at all, and a caller that fails silently
/// has every subsequent injected keyboard/mouse-button event go wherever focus already was
/// instead of `hwnd` — which looks, from the wire, exactly like a click or keystroke that was
/// dropped, not one that landed on the wrong window.
fn set_foreground(hwnd: HWND, handle: isize) {
    // SAFETY: `hwnd` is derived from a handle the driver resolved through its own window table
    // for this exact event; a handle that has since gone stale simply makes the call fail, which
    // is `BOOL(0)`, not unsound.
    let ok = unsafe { SetForegroundWindow(hwnd) };
    if !ok.as_bool() {
        // SAFETY: queried immediately after the call whose failure this explains; nothing
        // between the two calls can have overwritten the calling thread's last-error value.
        let err = unsafe { GetLastError() };
        eprintln!(
            "oxagent: input: SetForegroundWindow({handle:#x}) failed, GetLastError={}",
            err.0
        );
    }
}

impl InputSink for WinInputSink {
    fn pointer_event(
        &mut self,
        handle: isize,
        x: i32,
        y: i32,
        buttons: u8,
        wheel_x: i16,
        wheel_y: i16,
    ) {
        let hwnd = HWND(handle as *mut c_void);
        // SAFETY: `hwnd` is derived from a handle the driver resolved through its own window
        // table for this exact event; a handle that has since gone stale (the window closed
        // between the driver's last poll and this event arriving) simply fails the query below.
        let Some(rect) = (unsafe { enumerate::frame_bounds(hwnd) }) else {
            return;
        };

        // A newly pressed button (one that was not down a moment ago) is what "clicking into a
        // window" means; plain motion must never steal focus from whatever the user is doing.
        let newly_pressed = buttons & !self.last_buttons;
        if newly_pressed != 0 {
            self.focus_on_change(hwnd, handle);
        }

        // Window-relative -> guest screen coordinates, through the same extended frame bounds
        // capture uses (OXPROTO.md §6), so a click lands on the pixel the user actually sees.
        // `x`/`y` come straight off the wire from an authenticated-but-untrusted peer, so this
        // is saturating rather than plain `+`: the release profile does not enable
        // `overflow-checks`, so a crafted extreme value would not panic today, but turning that
        // hardening on later must not turn one bad message into a process crash post-auth.
        // `normalize` below clamps the result into the virtual desktop regardless.
        let abs_x = rect.left.saturating_add(x);
        let abs_y = rect.top.saturating_add(y);

        let (vx, vy, vw, vh) = virtual_desktop_rect();
        let norm_x = normalize(abs_x, vx, vw);
        let norm_y = normalize(abs_y, vy, vh);

        let mut flags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        let changed = buttons ^ self.last_buttons;
        if changed & pointer_button::LEFT != 0 {
            flags |= down_up(
                buttons & pointer_button::LEFT != 0,
                MOUSEEVENTF_LEFTDOWN,
                MOUSEEVENTF_LEFTUP,
            );
        }
        if changed & pointer_button::RIGHT != 0 {
            flags |= down_up(
                buttons & pointer_button::RIGHT != 0,
                MOUSEEVENTF_RIGHTDOWN,
                MOUSEEVENTF_RIGHTUP,
            );
        }
        if changed & pointer_button::MIDDLE != 0 {
            flags |= down_up(
                buttons & pointer_button::MIDDLE != 0,
                MOUSEEVENTF_MIDDLEDOWN,
                MOUSEEVENTF_MIDDLEUP,
            );
        }

        // Motion and the plain buttons ride in one MOUSEINPUT: none of them use `mouseData`, so
        // their flags can be OR-ed together and injected as a single event.
        let mut inputs = vec![mouse_input(norm_x, norm_y, 0, flags)];

        // X1/X2 and the wheel each need their own event: `mouseData` means something different
        // for each (which extra button, or the wheel delta), so they cannot share a MOUSEINPUT
        // with each other or with the buttons above.
        if changed & pointer_button::X1 != 0 {
            let f = down_up(
                buttons & pointer_button::X1 != 0,
                MOUSEEVENTF_XDOWN,
                MOUSEEVENTF_XUP,
            );
            inputs.push(mouse_input(0, 0, u32::from(XBUTTON1), f));
        }
        if changed & pointer_button::X2 != 0 {
            let f = down_up(
                buttons & pointer_button::X2 != 0,
                MOUSEEVENTF_XDOWN,
                MOUSEEVENTF_XUP,
            );
            inputs.push(mouse_input(0, 0, u32::from(XBUTTON2), f));
        }
        // The wire's wheel units (1/120 of a notch) are exactly `WHEEL_DELTA` units, Windows'
        // own definition of one notch — no scaling needed, just a sign-preserving cast.
        if wheel_y != 0 {
            inputs.push(mouse_input(0, 0, wheel_y as i32 as u32, MOUSEEVENTF_WHEEL));
        }
        if wheel_x != 0 {
            inputs.push(mouse_input(0, 0, wheel_x as i32 as u32, MOUSEEVENTF_HWHEEL));
        }

        self.last_buttons = buttons;
        send_inputs(&inputs);
    }

    fn key_event(&mut self, scancode: u16, extended: bool, pressed: bool) {
        send_inputs(&[scan_input(scancode, extended, pressed)]);
    }

    fn text_input(&mut self, text: &str) {
        let mut inputs = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            inputs.push(unicode_input(unit, false));
            inputs.push(unicode_input(unit, true));
        }
        send_inputs(&inputs);
    }

    fn modifier_sync(&mut self, modifiers: u16, locks: u8) {
        sync_held(
            modifiers & modifier::SHIFT != 0,
            Key::LeftShift,
            Key::RightShift,
            VK_LSHIFT,
            VK_RSHIFT,
        );
        sync_held(
            modifiers & modifier::CTRL != 0,
            Key::LeftControl,
            Key::RightControl,
            VK_LCONTROL,
            VK_RCONTROL,
        );
        sync_held(
            modifiers & modifier::ALT != 0,
            Key::LeftAlt,
            Key::RightAlt,
            VK_LMENU,
            VK_RMENU,
        );
        sync_held(
            modifiers & modifier::META != 0,
            Key::LeftMeta,
            Key::RightMeta,
            VK_LWIN,
            VK_RWIN,
        );

        sync_lock(locks & lock_state::CAPS != 0, Key::CapsLock, VK_CAPITAL);
        sync_lock(locks & lock_state::NUM != 0, Key::NumLock, VK_NUMLOCK);
        sync_lock(locks & lock_state::SCROLL != 0, Key::ScrollLock, VK_SCROLL);
    }

    fn window_control(
        &mut self,
        handle: isize,
        action: u8,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    ) {
        let hwnd = HWND(handle as *mut c_void);
        match action {
            window_action::CLOSE => {
                // SAFETY: `hwnd` is derived from a handle the driver resolved through its own
                // window table; posting to a handle that has since gone stale is a harmless
                // failure, not unsound.
                unsafe {
                    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            window_action::ACTIVATE => {
                // See the type-level doc: this always forces focus, never throttled on
                // `last_focused`.
                set_foreground(hwnd, handle);
                self.last_focused = Some(handle);
            }
            window_action::MINIMIZE => {
                // SAFETY: same as above.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
            }
            window_action::MAXIMIZE => {
                // SAFETY: same as above.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                }
            }
            window_action::RESTORE => {
                // SAFETY: same as above.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
            }
            window_action::MOVE => {
                // The z-order argument is `hwnd` itself only because `SWP_NOZORDER` makes it
                // ignored outright — reusing an already-valid handle avoids reaching for a
                // sentinel HWND just to satisfy the signature.
                // SAFETY: same as above.
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        hwnd,
                        x,
                        y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            window_action::RESIZE => {
                // SAFETY: same as above.
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        hwnd,
                        0,
                        0,
                        i32::from(width),
                        i32::from(height),
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            // An action code this build does not know is ignored, not a protocol error
            // (OXPROTO.md rule 6): it lets a newer client talk to an older agent.
            _ => {}
        }
    }
}

/// `down` if `held`, `up` otherwise — the recurring shape of every plain mouse button flag.
fn down_up(held: bool, down: MOUSE_EVENT_FLAGS, up: MOUSE_EVENT_FLAGS) -> MOUSE_EVENT_FLAGS {
    if held {
        down
    } else {
        up
    }
}

/// The virtual desktop's origin and extent: `(x, y, width, height)`. Read fresh on every call
/// since a monitor can be attached or removed mid-session.
fn virtual_desktop_rect() -> (i32, i32, i32, i32) {
    // SAFETY: `GetSystemMetrics` takes a plain metric index and returns a plain integer; there
    // is nothing here that can be unsound.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Map `value` (a screen coordinate on the axis whose origin is `origin` and whose extent is
/// `extent`) onto the normalized `0..=65535` range `MOUSEEVENTF_ABSOLUTE` requires, clamping
/// out-of-range input first so a client sending nonsense coordinates cannot warp the cursor
/// outside the virtual desktop.
fn normalize(value: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let last = origin.saturating_add(extent - 1);
    let clamped = value.clamp(origin, last);
    // i64 throughout: `65535 * extent` does not fit i32 for a large virtual desktop.
    (i64::from(clamped - origin) * 65535 / i64::from(extent - 1)) as i32
}

/// Query whether either physical variant of a modifier is currently held.
fn is_key_down(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: `GetAsyncKeyState` takes a plain virtual-key code; nothing here can be unsound.
    (unsafe { GetAsyncKeyState(i32::from(vk.0)) } as u16) & 0x8000 != 0
}

/// Query a toggle-style key's on/off state (caps/num/scroll lock).
fn is_toggled(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: `GetKeyState` takes a plain virtual-key code; nothing here can be unsound.
    (unsafe { GetKeyState(i32::from(vk.0)) } & 0x0001) != 0
}

/// Bring a held modifier to `want_down`, by physical key rather than guest-visible logical
/// state: pressing checks both variants so an already-held key is not pressed again, but
/// releasing always clears both, because `ModifierSync` cannot say *which* physical key is
/// stuck and releasing an already-up key is a harmless no-op.
fn sync_held(want_down: bool, left: Key, right: Key, vk_left: VIRTUAL_KEY, vk_right: VIRTUAL_KEY) {
    if want_down {
        if !is_key_down(vk_left) && !is_key_down(vk_right) {
            inject_key(left, true);
        }
    } else {
        inject_key(left, false);
        inject_key(right, false);
    }
}

/// Bring a toggle-style lock key to `want_on` by tapping it once if the guest disagrees.
fn sync_lock(want_on: bool, key: Key, vk: VIRTUAL_KEY) {
    if is_toggled(vk) != want_on {
        inject_key(key, true);
        inject_key(key, false);
    }
}

/// Inject a named key by its table scancode (`oxproto::scancode`) rather than duplicating the
/// PS/2 table here.
fn inject_key(key: Key, pressed: bool) {
    let sc = key.scancode();
    send_inputs(&[scan_input(sc.code, sc.extended, pressed)]);
}

/// Build one `KEYEVENTF_SCANCODE` input. `wVk` is left zero: scancode mode ignores it, and
/// setting it would be the virtual-key path this agent deliberately avoids.
fn scan_input(scancode: u16, extended: bool, pressed: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Build one `KEYEVENTF_UNICODE` input for a single UTF-16 code unit (a surrogate half counts
/// as one call; Windows recombines the pair itself).
fn unicode_input(utf16_unit: u16, key_up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: utf16_unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Build one mouse `INPUT`. `dx`/`dy` are only meaningful with `MOUSEEVENTF_ABSOLUTE` set in
/// `flags`; callers that only carry a button or wheel event pass `0, 0`.
fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Send a batch of already-built inputs, if there are any.
fn send_inputs(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: `inputs` is a slice of fully-initialized `INPUT` values; `size_of::<INPUT>()`
    // matches the type `SendInput` is being called with, as the API requires.
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}
