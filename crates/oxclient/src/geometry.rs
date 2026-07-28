//! Which local window-geometry changes are the user's intent, and what they mean on the guest.
//!
//! # The problem this exists to solve
//!
//! `oxdisplay` reports what the host window manager did: `ResizeRequested` when a native window
//! changes size, `MoveRequested` when it changes position. Those events are identical whether the
//! user dragged the window or the WM placed it — and a WM places and sizes every window the
//! moment it is created. Forwarding them all turned "connect to the guest" into "rearrange and
//! shrink every window on the guest", with host-screen coordinates written into a guest desktop
//! where they mean nothing.
//!
//! # The two rules
//!
//! 1. **A geometry change the client caused is not the user's intent.** Creating a window and
//!    applying the guest's own geometry both provoke WM events; both open a short settling
//!    window during which local reports only *update what the client believes*, and are never
//!    sent onward.
//! 2. **Only an observed displacement is ever sent, never an observed position.** A position is
//!    a host-screen coordinate, which has no meaning on the guest. A displacement is the same
//!    number in both spaces. The guest position the client sends is always
//!    `last known guest position + (how far the user just dragged it)`, so it needs two local
//!    observations to send one move — which makes "echo the WM's placement" structurally
//!    impossible rather than merely unlikely.
//!
//! Rule 2 is a deliberate deviation from `docs/design/client-display.md` §4 rule 2, which says
//! X11 positions map identically because winpodx mirrors the guest desktop onto the client's
//! layout. That premise does not hold today — the guest desktop observed in testing is 1280x800
//! while the client's X screen spans past x=3257 — and under it the identity mapping is what
//! pushed windows off the guest desktop entirely. Anchored displacement *is* the identity
//! mapping whenever the premise does hold, so this is compatible with the policy's end state
//! and safe before it arrives.
//!
//! Size needs no such translation: `docs/design/client-display.md` §4 rule 1 makes the local
//! size authoritative, and a size is not a coordinate in anybody's screen space.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::model::WindowModel;

/// How long after a client-caused geometry change local reports are treated as the window
/// manager settling rather than as the user acting.
///
/// Matches the deadline `oxdisplay` uses for suppressing echoes in the other direction. A window
/// manager places a window within a frame or two of mapping it; a user cannot see a window
/// appear, reach for it and drag it inside this window. Should a WM place a window later than
/// this, rule 2 above still holds — the first position observed only ever becomes an anchor.
pub const SETTLE: Duration = Duration::from_millis(750);

/// Per-window geometry state: what the client believes, and whether it is settled.
#[derive(Debug, Clone, Copy)]
struct WindowSync {
    /// Local reports before this instant are the window manager, not the user.
    settling_until: Instant,
    /// Guest-space position that `local_anchor` corresponds to.
    guest_anchor: (i32, i32),
    /// Local position last observed, or `None` until the WM has reported one.
    local_anchor: Option<(i32, i32)>,
    /// Local size last observed or applied.
    local_size: Option<(u16, u16)>,
}

/// Decides what a local geometry change should tell the guest.
#[derive(Debug, Default)]
pub struct GeometrySync {
    windows: HashMap<u32, WindowSync>,
}

impl GeometrySync {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A native window was just created from the guest's geometry.
    pub fn created(&mut self, now: Instant, window_id: u32, x: i32, y: i32, size: (u16, u16)) {
        self.windows.insert(
            window_id,
            WindowSync {
                settling_until: now + SETTLE,
                guest_anchor: (x, y),
                // Not seeded from the requested position: the WM is free to place the window
                // somewhere else entirely, and guessing here is exactly how a WM placement gets
                // mistaken for a drag.
                local_anchor: None,
                local_size: Some(size),
            },
        );
    }

    /// The guest moved or resized a window and the client is applying it locally.
    pub fn guest_moved(&mut self, now: Instant, window_id: u32, x: i32, y: i32, size: (u16, u16)) {
        let Some(state) = self.windows.get_mut(&window_id) else {
            return;
        };
        state.settling_until = now + SETTLE;
        state.guest_anchor = (x, y);
        state.local_anchor = None;
        state.local_size = Some(size);
    }

    /// Drops a window's state.
    pub fn forget(&mut self, window_id: u32) {
        self.windows.remove(&window_id);
    }

    /// A native window reports a new position.
    ///
    /// Returns the guest-space position to send, or `None` when this is not the user moving the
    /// window.
    pub fn moved(&mut self, now: Instant, window_id: u32, x: i32, y: i32) -> Option<(i32, i32)> {
        let state = self.windows.get_mut(&window_id)?;
        let settling = now < state.settling_until;
        let previous = state.local_anchor.replace((x, y));

        // Rule 1: the window manager is still placing this window.
        if settling {
            return None;
        }
        // Rule 2: one observation is an anchor, not a displacement. Nothing to send yet.
        let (previous_x, previous_y) = previous?;
        let dx = x.saturating_sub(previous_x);
        let dy = y.saturating_sub(previous_y);
        if dx == 0 && dy == 0 {
            return None;
        }

        let target = (
            state.guest_anchor.0.saturating_add(dx),
            state.guest_anchor.1.saturating_add(dy),
        );
        // The user's drag is now the truth for both spaces.
        state.guest_anchor = target;
        Some(target)
    }

    /// A native window reports a new size.
    ///
    /// Returns the size to ask the guest for, or `None` when the client must not ask.
    pub fn resized(
        &mut self,
        now: Instant,
        model: &WindowModel,
        window_id: u32,
        width: u16,
        height: u16,
    ) -> Option<(u16, u16)> {
        // A window the guest reports as fixed-size is a dialog like charmap: asking it to
        // resize either does nothing or corrupts it, and the WM should not have offered.
        if !model.get(window_id).is_some_and(|window| window.resizable) {
            return None;
        }
        // A zero axis is an unmapped or minimised window, not a resize to nothing.
        if width == 0 || height == 0 {
            return None;
        }

        let state = self.windows.get_mut(&window_id)?;
        let settling = now < state.settling_until;
        let unchanged = state.local_size == Some((width, height));
        state.local_size = Some((width, height));

        if settling || unchanged {
            return None;
        }
        Some((width, height))
    }
}

#[cfg(test)]
mod tests {
    use oxproto::message::window::{window_flag, window_show};
    use oxproto::message::{WindowGeometry, WindowOpened, WindowState};

    use super::*;
    use crate::session::ClientEvent;

    const RESIZABLE: u32 = window_flag::RESIZABLE | window_flag::HAS_FRAME;
    const FIXED: u32 = window_flag::HAS_FRAME;

    fn model_with(
        window_id: u32,
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        flags: u32,
    ) -> WindowModel {
        let mut model = WindowModel::new();
        model.apply(ClientEvent::WindowOpened(WindowOpened {
            window_id,
            video_channel: 16,
            pid: 1,
            app_id: "app.exe".into(),
            title: "app".into(),
            x,
            y,
            width,
            height,
            dpi: 96,
            flags,
            owner_id: 0,
        }));
        model
    }

    /// A window created, then settled, with the WM having placed it at `placed`.
    fn settled(
        window_id: u32,
        guest: (i32, i32),
        size: (u16, u16),
        placed: (i32, i32),
    ) -> (GeometrySync, Instant) {
        let start = Instant::now();
        let mut sync = GeometrySync::new();
        sync.created(start, window_id, guest.0, guest.1, size);
        // The window manager places the window; this is not a gesture.
        assert_eq!(sync.moved(start, window_id, placed.0, placed.1), None);
        (sync, start + SETTLE + Duration::from_millis(1))
    }

    #[test]
    fn a_freshly_created_window_sends_no_geometry() {
        let model = model_with(1, 100, 200, 800, 600, RESIZABLE);
        let start = Instant::now();
        let mut sync = GeometrySync::new();

        sync.created(start, 1, 100, 200, (800, 600));

        // Everything the host window manager does while placing the window is silent: the
        // position it chose, the size it chose, and both again a moment later.
        assert_eq!(sync.moved(start, 1, 3257, 2262), None);
        assert_eq!(sync.resized(start, &model, 1, 122, 47), None);
        let later = start + Duration::from_millis(200);
        assert_eq!(sync.moved(later, 1, 3214, 156), None);
        assert_eq!(sync.resized(later, &model, 1, 466, 77), None);
    }

    #[test]
    fn a_user_drag_after_settling_sends_a_guest_space_position() {
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (3257, 2262));

        // The user drags the window 40 right and 15 up. What the guest is told is its own
        // position moved by that much — never the host coordinate.
        let target = sync.moved(now, 1, 3297, 2247);

        assert_eq!(target, Some((140, 185)));
    }

    #[test]
    fn a_host_screen_position_is_never_sent_as_a_guest_position() {
        let (mut sync, now) = settled(1, (10, 20), (800, 600), (3257, 2262));

        // Whatever the user does, the number sent is anchored to the guest's own last position.
        // The host coordinates here are far outside any plausible guest desktop.
        let first = sync.moved(now, 1, 3260, 2264).expect("a drag is reported");
        let second = sync
            .moved(now + Duration::from_millis(10), 1, 3250, 2260)
            .expect("a second drag is reported");

        assert_eq!(first, (13, 22));
        assert_eq!(second, (3, 18));
    }

    #[test]
    fn the_first_position_after_settling_is_an_anchor_not_a_move() {
        let start = Instant::now();
        let mut sync = GeometrySync::new();
        sync.created(start, 1, 100, 200, (800, 600));
        let now = start + SETTLE + Duration::from_millis(1);

        // A window manager that reported nothing at all during settling must still not be able
        // to turn its first report into a move.
        assert_eq!(sync.moved(now, 1, 3257, 2262), None);
        assert_eq!(sync.moved(now, 1, 3267, 2262), Some((110, 200)));
    }

    #[test]
    fn applying_guest_geometry_does_not_bounce_back() {
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (100, 200));

        // The guest moves its own window; the client applies it and the WM reports the result.
        sync.guest_moved(now, 1, 640, 480, (800, 600));
        assert_eq!(sync.moved(now, 1, 640, 480), None);

        // And the settling window closes again afterwards.
        let later = now + SETTLE + Duration::from_millis(1);
        assert_eq!(sync.moved(later, 1, 650, 480), Some((650, 480)));
    }

    #[test]
    fn a_window_that_never_moves_sends_nothing() {
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (300, 400));

        // Repeated reports of the same position are not drags.
        assert_eq!(sync.moved(now, 1, 300, 400), None);
        assert_eq!(sync.moved(now, 1, 300, 400), None);
    }

    #[test]
    fn a_non_resizable_window_never_asks_the_guest_to_resize() {
        // charmap: a fixed-size dialog that reports has_frame without resizable.
        let model = model_with(1, 100, 200, 322, 197, FIXED);
        let (mut sync, now) = settled(1, (100, 200), (322, 197), (259, 2262));

        assert_eq!(sync.resized(now, &model, 1, 1, 52), None);
        assert_eq!(sync.resized(now, &model, 1, 640, 480), None);
    }

    #[test]
    fn a_resizable_window_reports_a_real_user_resize() {
        let model = model_with(1, 100, 200, 800, 600, RESIZABLE);
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (100, 200));

        assert_eq!(sync.resized(now, &model, 1, 1024, 768), Some((1024, 768)));
        // Repeats of the size just reported are not new intent.
        assert_eq!(sync.resized(now, &model, 1, 1024, 768), None);
    }

    #[test]
    fn a_zero_sized_report_is_never_a_resize() {
        let model = model_with(1, 100, 200, 800, 600, RESIZABLE);
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (100, 200));

        // Some window managers report 0x0 when a window is unmapped or minimised. Forwarding
        // that would ask the guest to resize its window to nothing.
        assert_eq!(sync.resized(now, &model, 1, 0, 0), None);
        assert_eq!(sync.resized(now, &model, 1, 1024, 0), None);
        assert_eq!(sync.resized(now, &model, 1, 0, 768), None);
    }

    #[test]
    fn resizability_is_read_from_the_model_not_cached_at_creation() {
        // The tracker holds no copy of the flag: it asks the model every time, so the day
        // `WindowState` grows a way to carry a post-open flag change (`flags` is reserved on the
        // wire today, and the agent sends 0) this follows without touching this file.
        let resizable = model_with(1, 100, 200, 800, 600, RESIZABLE);
        let fixed = model_with(1, 100, 200, 800, 600, FIXED);
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (100, 200));

        assert_eq!(sync.resized(now, &fixed, 1, 1024, 768), None);
        assert_eq!(
            sync.resized(now, &resizable, 1, 1024, 768),
            Some((1024, 768))
        );
    }

    #[test]
    fn a_show_state_change_does_not_make_a_fixed_window_resizable() {
        let mut model = model_with(1, 100, 200, 322, 197, FIXED);
        let (mut sync, now) = settled(1, (100, 200), (322, 197), (100, 200));

        // Minimising and restoring is the one state change the guest does send, and it must not
        // turn charmap into something the client may resize.
        model.apply(ClientEvent::WindowState(WindowState {
            window_id: 1,
            state: window_show::MINIMIZED,
            flags: 0,
        }));
        model.apply(ClientEvent::WindowState(WindowState {
            window_id: 1,
            state: window_show::NORMAL,
            flags: 0,
        }));

        assert_eq!(sync.resized(now, &model, 1, 640, 480), None);
    }

    #[test]
    fn an_unknown_window_is_silent() {
        let model = model_with(1, 0, 0, 800, 600, RESIZABLE);
        let mut sync = GeometrySync::new();
        let now = Instant::now();

        assert_eq!(sync.moved(now, 9, 10, 10), None);
        assert_eq!(sync.resized(now, &model, 9, 10, 10), None);
    }

    #[test]
    fn a_forgotten_window_is_silent() {
        let model = model_with(1, 100, 200, 800, 600, RESIZABLE);
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (100, 200));

        sync.forget(1);

        assert_eq!(sync.moved(now, 1, 500, 500), None);
        assert_eq!(sync.resized(now, &model, 1, 640, 480), None);
    }

    #[test]
    fn windows_do_not_share_anchors() {
        let start = Instant::now();
        let mut sync = GeometrySync::new();
        sync.created(start, 1, 100, 100, (800, 600));
        sync.created(start, 2, 500, 500, (400, 300));
        assert_eq!(sync.moved(start, 1, 0, 0), None);
        assert_eq!(sync.moved(start, 2, 2000, 2000), None);
        let now = start + SETTLE + Duration::from_millis(1);

        assert_eq!(sync.moved(now, 1, 10, 5), Some((110, 105)));
        assert_eq!(sync.moved(now, 2, 1990, 2000), Some((490, 500)));
    }

    #[test]
    fn extreme_host_coordinates_cannot_overflow_the_guest_position() {
        let (mut sync, now) = settled(1, (i32::MAX, i32::MIN), (800, 600), (0, 0));

        let target = sync.moved(now, 1, i32::MAX, i32::MIN).expect("a drag");

        assert_eq!(target, (i32::MAX, i32::MIN));
    }

    #[test]
    fn geometry_events_track_the_model_after_a_guest_geometry_update() {
        let mut model = model_with(1, 100, 200, 800, 600, RESIZABLE);
        let (mut sync, now) = settled(1, (100, 200), (800, 600), (300, 300));

        // The guest reports its own move; the model and the tracker are updated together, the
        // way the session task does it.
        model.apply(ClientEvent::WindowGeometry(WindowGeometry {
            window_id: 1,
            x: 640,
            y: 480,
            width: 800,
            height: 600,
        }));
        let window = model.get(1).expect("the window is still open");
        sync.guest_moved(now, 1, window.x, window.y, (window.width, window.height));

        // The WM echo of that move is silent, and a later drag is measured from the guest's new
        // position rather than the one it had before.
        assert_eq!(sync.moved(now, 1, 700, 700), None);
        let later = now + SETTLE + Duration::from_millis(1);
        assert_eq!(sync.moved(later, 1, 705, 690), Some((645, 470)));
    }
}
