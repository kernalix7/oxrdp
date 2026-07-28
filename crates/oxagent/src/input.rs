//! What the session driver needs from the platform to inject input
//! (`docs/design/OXPROTO.md` §13).
//!
//! Mirrors [`crate::serve::WindowSource`]: the platform sits behind a trait so the driver's
//! dispatch — resolving a wire `window_id` to the window it actually means, deciding which
//! messages a negotiated feature actually allows — is exercised on the Linux build host where
//! CI runs. Only the Windows implementation (`crate::win::input::WinInputSink`) touches
//! `SendInput`.

/// Injects input into the guest on behalf of the session driver.
///
/// Every method that targets a window takes an already-resolved native `handle`, never a wire
/// `window_id`: [`crate::serve::run_session`]'s driver looks the id up in the window table
/// first, so an event for a window this session never announced (or has already closed) never
/// reaches a sink at all, rather than landing on whatever window happens to be in the
/// foreground.
///
/// Implementations must not block; the driver's tick is also its pacing clock.
pub trait InputSink {
    /// Pointer motion, held buttons, and wheel deltas for the window at `handle`. `x`/`y` are
    /// window-relative, exactly as received on the wire (`OXPROTO.md` §6, §13); the sink owns
    /// whatever coordinate conversion injection needs. `buttons` is the bitmask of currently
    /// held buttons ([`oxproto::message::input::pointer_button`]), not a delta. `wheel_x` and
    /// `wheel_y` are in 1/120-of-a-notch units.
    fn pointer_event(
        &mut self,
        handle: isize,
        x: i32,
        y: i32,
        buttons: u8,
        wheel_x: i16,
        wheel_y: i16,
    );

    /// A key press or release. `scancode`/`extended` are PS/2 set-1, passed through unchanged
    /// from the wire — never translated through a keysym or virtual key, which is what makes
    /// injection independent of the guest's *and* the client's keyboard layout. See
    /// `oxproto::scancode`.
    fn key_event(&mut self, scancode: u16, extended: bool, pressed: bool);

    /// Literal Unicode text (IME/emoji path) that has no scancode representation. Only called
    /// when both peers negotiated `TEXT_INPUT`.
    fn text_input(&mut self, text: &str);

    /// Authoritative modifier/lock state ([`oxproto::message::input::modifier`],
    /// [`oxproto::message::input::lock_state`]), sent on client focus change and periodically
    /// so a modifier released while the client window was unfocused cannot leave the guest
    /// with a stuck key.
    fn modifier_sync(&mut self, modifiers: u16, locks: u8);

    /// A client-initiated window action
    /// ([`oxproto::message::input::window_action`]: close/activate/minimize/maximize/restore/
    /// move/resize) for the window at `handle`. Only called when both peers negotiated
    /// `WINDOW_CONTROL`.
    fn window_control(
        &mut self,
        handle: isize,
        action: u8,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
    );
}
