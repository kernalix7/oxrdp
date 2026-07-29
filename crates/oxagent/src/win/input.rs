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
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
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
    BringWindowToTop, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId,
    PostMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, WM_CLOSE, XBUTTON1, XBUTTON2,
};

use super::enumerate;
use crate::input::{gate_unconfirmed_press, InputSink};

/// Injects input on the Windows guest via `SendInput`.
///
/// # Focus
/// `SendInput` has no "inject into this HWND" primitive: injected keyboard input, and any
/// mouse-button injection, always goes to whatever window currently has Win32 focus, and mouse
/// motion/clicks are hit-tested purely by screen coordinate against whatever window is topmost
/// in z-order there — the actual target `HWND` is never consulted by `SendInput` at all. So a
/// session driving one particular window must both bring it to the foreground *and* have that
/// confirmed to have actually happened, before injecting a click meant for it; otherwise the
/// click lands on whatever window Windows really left on top, silently, which looks from the
/// wire exactly like a correctly-targeted click. (This was a real, observed bug: a click at the
/// right coordinates for the right window landed in an unrelated window stacked above it on the
/// guest, because raising the target had silently failed.)
///
/// This struct raises a window only on a state change — the first pointer button-press edge that
/// targets a window other than the one last confirmed focused
/// ([`WinInputSink::pointer_event`]), or an explicit `WindowControl::ACTIVATE` — never on plain
/// motion, so passive mouse movement over a window never steals focus and this does not fight a
/// user physically at the guest console on every event. Should a click raise its target at all,
/// though? On a real desktop, yes: clicking a background window raises it, and every
/// `PointerEvent` this receives already represents the *client's* full attention on that
/// specific window, addressed by `window_id` — stronger evidence of intent than an ordinary
/// desktop click gets, since the guest's internal z-order is an implementation detail the
/// client-side user cannot even see, on a client that streams several guest windows into
/// separate native windows of their own. So "any window switch forces a raise" (what this
/// already does) is the right amount of force, not a half-measure: there is no meaningful sense
/// in which a *more* aggressive policy (raising on every event, not just a switch) would be more
/// correct — it would only add z-order flicker for zero addressing benefit, since motion and
/// wheel deltas carry no targeting ambiguity `SendInput` needs help resolving.
///
/// A plain `SetForegroundWindow` is not enough to make any of this reliable: Windows' anti-
/// focus-stealing heuristic refuses it from a process that "didn't just receive input," which an
/// injection agent never has on its own — see [`force_foreground`] for the `AttachThreadInput`
/// fallback and how its success is verified rather than assumed.
#[derive(Debug, Default)]
pub struct WinInputSink {
    /// Handle last *confirmed* focused (see [`force_foreground`]), so repeated clicks inside the
    /// window that already has focus do not re-issue `SetForegroundWindow`, and so a raise that
    /// silently failed is not misremembered as having succeeded — the latter used to be exactly
    /// this field's bug: it recorded the target as focused unconditionally, before its own
    /// discarded return value was even checked.
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
    /// Returns whether `hwnd` is now confirmed foreground either way. On failure, `last_focused`
    /// is deliberately left untouched (not set to `handle`) so the next call — the very next
    /// tick, if the client's button is still held, since the wire keeps reporting the same
    /// press — retries from the same state instead of wrongly remembering a focus change that
    /// never happened.
    fn focus_on_change(&mut self, hwnd: HWND, handle: isize) -> bool {
        if self.last_focused == Some(handle) {
            return true;
        }
        let confirmed = force_foreground(hwnd, handle);
        if confirmed {
            self.last_focused = Some(handle);
        }
        confirmed
    }
}

/// Bring `hwnd` to the foreground, retrying through `AttachThreadInput` if a plain
/// `SetForegroundWindow` is refused, and confirm which (if either) actually worked. Returns
/// whether `hwnd` is now the foreground window.
///
/// `SetForegroundWindow`'s `BOOL` return used to be discarded outright with `let _ =`. That
/// matters more than most discarded booleans here: Windows refuses a bare `SetForegroundWindow`
/// from a process that "didn't just receive input" — the documented anti-focus-stealing
/// heuristic every modern Windows enforces — and an injection agent calling it out of the blue,
/// on a click that arrived over the network rather than through the guest's own input queue, is
/// exactly the case that heuristic exists to deny. A caller that fails silently there has every
/// subsequent injected keyboard/mouse-button event go wherever focus already was instead of
/// `hwnd` — which looks, from the wire, exactly like input that landed on the *wrong* window,
/// not input that was dropped, since nothing about the injection itself fails.
///
/// The fallback is the standard, documented way around that heuristic, without touching any
/// system-wide policy or lowering any process's privilege: `AttachThreadInput` lets this thread
/// temporarily share input state with the thread that owns the *current* foreground window. Once
/// attached, this thread is — as far as the heuristic can tell — the same thread that just
/// legitimately had input, so its own `SetForegroundWindow` call is no longer "out of the blue"
/// and is let through. `BringWindowToTop` alongside it is belt-and-suspenders: the documented
/// idiom pairs the two, and this crate has no live Windows session to confirm
/// `SetForegroundWindow` alone is always sufficient for z-order once attached, only that pairing
/// them is not documented to be wrong.
///
/// # Why checking `GetForegroundWindow()` right after is not a race
/// What a click needs true before it is safe to inject is "this window is now the foreground
/// window and on top of the z-order at the click's screen coordinates" — not "this window's own
/// message loop has processed `WM_ACTIVATE`". The first two are applied synchronously, inside
/// the `SetForegroundWindow`/`BringWindowToTop` calls themselves, as part of the window
/// manager's shared state; only the *notification* to the window being activated
/// (`WM_ACTIVATE`/`WM_SETFOCUS`, posted to its own queue for its own thread to process whenever
/// it next pumps messages) is asynchronous, and this needs none of that to inject correctly.
/// `GetForegroundWindow` reads the same shared state `SetForegroundWindow` just wrote, on this
/// thread, without waiting for the target's message loop to run at all — a direct read of the
/// fact this cares about, not a guess at when some other thread gets around to it.
fn force_foreground(hwnd: HWND, handle: isize) -> bool {
    // SAFETY: takes no arguments and cannot fail.
    if unsafe { GetForegroundWindow() } == hwnd {
        return true;
    }

    // SAFETY: `hwnd` is derived from a handle the driver resolved through its own window table
    // for this exact event; a handle that has since gone stale simply makes the call fail, which
    // is `BOOL(0)`, not unsound.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }

    // SAFETY: takes no arguments and cannot fail.
    let mut current = unsafe { GetForegroundWindow() };
    if current != hwnd {
        // The plain attempt above was refused — see this function's doc for why, and why
        // `AttachThreadInput` is the fix rather than a workaround for a workaround.
        // SAFETY: `current` was just queried live; an invalid/null HWND (no foreground window
        // at all) is a documented, harmless input to `GetWindowThreadProcessId` — it returns 0,
        // it does not fault. `None` for the process-id out-param means only the thread id, the
        // function's ordinary return value, is wanted.
        let fg_thread = unsafe { GetWindowThreadProcessId(current, None) };
        // SAFETY: takes no arguments and cannot fail.
        let this_thread = unsafe { GetCurrentThreadId() };
        if fg_thread != 0 && fg_thread != this_thread {
            // SAFETY: both thread ids were just queried live. The detach below runs
            // unconditionally once attach succeeds, on the same thread, before this function
            // returns, so the attachment is never left outstanding.
            if unsafe { AttachThreadInput(this_thread, fg_thread, true) }.as_bool() {
                // SAFETY: `hwnd` as above; `this_thread`/`fg_thread` are the pair just attached.
                unsafe {
                    let _ = BringWindowToTop(hwnd);
                    let _ = SetForegroundWindow(hwnd);
                    let _ = AttachThreadInput(this_thread, fg_thread, false);
                }
            }
        }
        // SAFETY: takes no arguments and cannot fail.
        current = unsafe { GetForegroundWindow() };
    }

    let confirmed = current == hwnd;
    if !confirmed {
        // SAFETY: queried immediately after the sequence whose failure this explains; nothing
        // between the two calls can have overwritten the calling thread's last-error value.
        let err = unsafe { GetLastError() };
        eprintln!(
            "oxagent: input: could not bring {handle:#x} to the foreground, GetLastError={}",
            err.0
        );
    }
    confirmed
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
        // A newly-pressed edge whose raise cannot be confirmed is gated out below — injecting it
        // anyway would land on whatever window Windows actually left on top instead, not the
        // one the client addressed (see the type-level doc's "Focus" section).
        let newly_pressed = buttons & !self.last_buttons;
        let focus_confirmed = if newly_pressed != 0 {
            self.focus_on_change(hwnd, handle)
        } else {
            true
        };
        let buttons = gate_unconfirmed_press(buttons, self.last_buttons, focus_confirmed);

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
                // See the type-level doc: this always attempts the raise, never throttled on
                // `last_focused` — it is itself the client's explicit "bring this forward"
                // request. `last_focused` is only updated on confirmed success, same as
                // `focus_on_change`, so a failed raise here does not stop a later click on this
                // same window from retrying.
                if force_foreground(hwnd, handle) {
                    self.last_focused = Some(handle);
                }
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
///
/// `SendInput`'s return is not a success boolean — it is the count of events it actually queued,
/// which can be less than `inputs.len()` (including zero) without the call itself failing. That
/// is the sharpest available signal for "the guest is silently swallowing input": a click or
/// keystroke that never arrives looks, from the client's side, identical whether the agent never
/// called `SendInput` at all or called it and had the queue reject every event, and only this
/// return value tells the two apart. A common real cause for a nonzero shortfall is UIPI
/// (`GetLastError` = `ERROR_ACCESS_DENIED`) blocking synthetic input into a window running at a
/// higher integrity level than this process.
fn send_inputs(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    // SAFETY: `inputs` is a slice of fully-initialized `INPUT` values; `size_of::<INPUT>()`
    // matches the type `SendInput` is being called with, as the API requires.
    let queued = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if (queued as usize) < inputs.len() {
        // SAFETY: queried immediately after the call whose shortfall this explains; nothing
        // between the two calls can have overwritten the calling thread's last-error value.
        let err = unsafe { GetLastError() };
        eprintln!(
            "oxagent: input: SendInput queued {queued}/{} events, GetLastError={}",
            inputs.len(),
            err.0
        );
    }
}
