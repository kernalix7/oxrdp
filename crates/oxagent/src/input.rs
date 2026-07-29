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

/// Which of `buttons`' bits a pointer sink should actually inject, given whether the window a
/// newly-pressed edge targets could be confirmed focused.
///
/// A free function, not a method, so this one piece of the click-targeting logic — the part with
/// no platform dependency at all — is exercised on the Linux build host like everything else in
/// this crate that can be. `crate::win::input::WinInputSink::pointer_event` is the only caller: a
/// click that raises a window to the foreground before injecting into it (see that type's doc
/// comment) can have the raise fail, and injecting the button-down anyway would land it on
/// whatever guest window Windows actually left on top at those screen coordinates — not the one
/// the client addressed, and not distinguishable from a correctly-targeted click on the wire.
/// Only the bits a *newly pressed* edge would assert are ever held back; a button that was
/// already legitimately down, or one being released, always goes through regardless of
/// `focus_confirmed` — releasing input is never the kind of mistake a misdirected click is.
pub fn gate_unconfirmed_press(buttons: u8, last_buttons: u8, focus_confirmed: bool) -> u8 {
    let newly_pressed = buttons & !last_buttons;
    if newly_pressed != 0 && !focus_confirmed {
        buttons & !newly_pressed
    } else {
        buttons
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirmed_press_is_reported_unchanged() {
        assert_eq!(gate_unconfirmed_press(0b001, 0b000, true), 0b001);
    }

    #[test]
    fn an_unconfirmed_new_press_is_withheld() {
        // Left button was up, is now reported down, but the target window's raise could not be
        // confirmed: the down bit must not reach the sink.
        assert_eq!(gate_unconfirmed_press(0b001, 0b000, false), 0b000);
    }

    #[test]
    fn an_unconfirmed_press_withholds_only_the_new_bit() {
        // Left was already down (and stays down); right is the new, unconfirmed press. Left
        // must survive the gate untouched.
        assert_eq!(gate_unconfirmed_press(0b011, 0b001, false), 0b001);
    }

    #[test]
    fn a_release_is_never_gated_even_when_unconfirmed() {
        // No new press here at all — buttons is a strict subset of last_buttons — so
        // `focus_confirmed` must not matter.
        assert_eq!(gate_unconfirmed_press(0b000, 0b001, false), 0b000);
    }

    #[test]
    fn an_unconfirmed_press_alongside_an_unrelated_release_still_only_gates_the_press() {
        // Right was down and is being released; left is a new, unconfirmed press. The release
        // must go through even though the press next to it is withheld.
        assert_eq!(gate_unconfirmed_press(0b001, 0b010, false), 0b000);
    }
}
