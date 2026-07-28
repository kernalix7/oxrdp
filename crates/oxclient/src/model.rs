//! Remote-window state, independent of any display backend.
//!
//! [`WindowModel`] turns the raw [`ClientEvent`] stream into an ordered list of
//! [`ModelChange`]s — "create this native window", "retitle it", "restack it" — so a backend
//! only has to execute instructions instead of diffing protocol messages itself. Every
//! backend (X11, Wayland, a headless test harness) consumes the same list, which is why this
//! lives below the backend boundary and is unit-tested with no windowing system present.
//!
//! The model deliberately does **not** retain frame pixels. Frames are large, arrive at video
//! rate, and are consumed immediately by the renderer; buffering them here would add a copy
//! and a queue to the one path where neither is affordable.

use std::collections::HashMap;

use oxproto::message::window::{window_flag, window_show};
use oxproto::message::{
    CursorShape, FrameData, WindowGeometry, WindowIcon, WindowOpened, WindowState, WindowTitle,
};

use crate::session::ClientEvent;

/// Everything the display layer knows about one remote window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWindow {
    /// Protocol id, stable for the session.
    pub window_id: u32,
    /// Channel this window's frames arrive on.
    pub video_channel: u16,
    /// Executable base name — the value a backend should use for `WM_CLASS`.
    pub app_id: String,
    /// Current title.
    pub title: String,
    /// Guest-side position of the visible frame.
    pub x: i32,
    /// Guest-side position of the visible frame.
    pub y: i32,
    /// Visible frame width; matches the captured frame size.
    pub width: u16,
    /// Visible frame height; matches the captured frame size.
    pub height: u16,
    /// Guest DPI for this window.
    pub dpi: u16,
    /// Owning window, or 0 — a backend maps this to transient-for so dialogs stack correctly.
    pub owner_id: u32,
    /// Whether the guest reports the window minimized.
    pub minimized: bool,
    /// Whether the guest reports the window maximized.
    pub maximized: bool,
    /// Whether the guest window can be resized by the user. A backend uses this to decide
    /// whether its native window is resizable — without it every window becomes fixed-size or
    /// every window becomes resizable, and both are wrong for some app.
    pub resizable: bool,
    /// Whether the guest window has a system frame. A backend that draws its own decoration
    /// for a frameless window (a tooltip, a menu) would put a title bar where the app expects
    /// none.
    pub has_frame: bool,
    /// Whether the guest keeps the window above others.
    pub topmost: bool,
    /// Latest icon, if the agent has sent one.
    pub icon: Option<WindowIcon>,
}

impl RemoteWindow {
    fn from_opened(m: &WindowOpened) -> Self {
        Self {
            window_id: m.window_id,
            video_channel: m.video_channel,
            app_id: m.app_id.clone(),
            title: m.title.clone(),
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            dpi: m.dpi,
            owner_id: m.owner_id,
            minimized: m.flags & window_flag::MINIMIZED != 0,
            maximized: m.flags & window_flag::MAXIMIZED != 0,
            resizable: m.flags & window_flag::RESIZABLE != 0,
            has_frame: m.flags & window_flag::HAS_FRAME != 0,
            topmost: m.flags & window_flag::TOPMOST != 0,
            icon: None,
        }
    }
}

/// An instruction for the display backend.
///
/// Emitted in the order it must be applied. A backend that executes these in order ends up
/// consistent with the guest without ever inspecting a protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChange {
    /// Create a native window for this remote window.
    Created(u32),
    /// The window is gone; destroy its native window.
    Destroyed(u32),
    /// Position and/or size changed.
    Moved(u32),
    /// The title changed.
    Retitled(u32),
    /// Minimized/maximized/restored.
    StateChanged(u32),
    /// A new icon is available.
    IconChanged(u32),
    /// Stacking changed; the backend should restack to match [`WindowModel::stack`].
    Restacked,
    /// A frame is ready to present. The pixels travel with the event, not in the model.
    Frame(FrameData),
    /// The cursor bitmap changed.
    CursorShape(CursorShape),
    /// The cursor moved within a window.
    CursorMoved {
        /// Window the cursor is over.
        window_id: u32,
        /// Window-relative position.
        x: i32,
        /// Window-relative position.
        y: i32,
    },
    /// The cursor was shown or hidden.
    CursorVisibility(bool),
    /// The agent reported an error; surface it and keep going.
    AgentError {
        /// Protocol error code.
        code: u16,
        /// Human-readable message from the agent.
        message: String,
    },
    /// The session is over.
    Closed,
}

/// The client's view of the guest's windows.
#[derive(Debug, Default)]
pub struct WindowModel {
    windows: HashMap<u32, RemoteWindow>,
    /// Bottom-to-top stacking order.
    stack: Vec<u32>,
    cursor_visible: bool,
}

impl WindowModel {
    /// An empty model.
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            stack: Vec::new(),
            cursor_visible: true,
        }
    }

    /// A window by id.
    pub fn get(&self, window_id: u32) -> Option<&RemoteWindow> {
        self.windows.get(&window_id)
    }

    /// Number of windows currently mapped.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether no windows are mapped.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Stacking order, bottom to top.
    pub fn stack(&self) -> &[u32] {
        &self.stack
    }

    /// Whether the cursor should be drawn.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Apply one event, returning the changes a backend must perform.
    ///
    /// An event for a window the model has never seen is ignored rather than treated as an
    /// error: the agent may legitimately send a geometry update racing with a close.
    pub fn apply(&mut self, event: ClientEvent) -> Vec<ModelChange> {
        match event {
            ClientEvent::WindowOpened(m) => self.on_opened(m),
            ClientEvent::WindowClosed(m) => self.on_closed(m.window_id),
            ClientEvent::WindowGeometry(m) => self.on_geometry(m),
            ClientEvent::WindowTitle(m) => self.on_title(m),
            ClientEvent::WindowState(m) => self.on_state(m),
            ClientEvent::WindowZOrder(m) => self.on_zorder(m.window_id, m.above_window_id),
            ClientEvent::WindowIcon(m) => self.on_icon(m),
            ClientEvent::Frame(f) => vec![ModelChange::Frame(f)],
            ClientEvent::CursorShape(c) => vec![ModelChange::CursorShape(c)],
            ClientEvent::CursorPosition(p) => vec![ModelChange::CursorMoved {
                window_id: p.window_id,
                x: p.x,
                y: p.y,
            }],
            ClientEvent::CursorVisibility(v) => {
                self.cursor_visible = v.visible;
                vec![ModelChange::CursorVisibility(v.visible)]
            }
            ClientEvent::Error(e) => vec![ModelChange::AgentError {
                code: e.code,
                message: e.message,
            }],
            ClientEvent::Closed(_) => vec![ModelChange::Closed],
        }
    }

    fn on_opened(&mut self, m: WindowOpened) -> Vec<ModelChange> {
        let id = m.window_id;
        // Ids are never reused within a session, so a repeat is a protocol violation; keep the
        // first and ignore the duplicate rather than orphaning a native window.
        if self.windows.contains_key(&id) {
            return Vec::new();
        }
        self.windows.insert(id, RemoteWindow::from_opened(&m));
        self.stack.push(id);
        vec![ModelChange::Created(id)]
    }

    fn on_closed(&mut self, id: u32) -> Vec<ModelChange> {
        if self.windows.remove(&id).is_none() {
            return Vec::new();
        }
        self.stack.retain(|&s| s != id);
        vec![ModelChange::Destroyed(id)]
    }

    fn on_geometry(&mut self, m: WindowGeometry) -> Vec<ModelChange> {
        let Some(w) = self.windows.get_mut(&m.window_id) else {
            return Vec::new();
        };
        if (w.x, w.y, w.width, w.height) == (m.x, m.y, m.width, m.height) {
            return Vec::new();
        }
        w.x = m.x;
        w.y = m.y;
        w.width = m.width;
        w.height = m.height;
        vec![ModelChange::Moved(m.window_id)]
    }

    fn on_title(&mut self, m: WindowTitle) -> Vec<ModelChange> {
        let Some(w) = self.windows.get_mut(&m.window_id) else {
            return Vec::new();
        };
        if w.title == m.title {
            return Vec::new();
        }
        w.title = m.title;
        vec![ModelChange::Retitled(m.window_id)]
    }

    fn on_state(&mut self, m: WindowState) -> Vec<ModelChange> {
        let Some(w) = self.windows.get_mut(&m.window_id) else {
            return Vec::new();
        };
        let minimized = m.state == window_show::MINIMIZED;
        let maximized = m.state == window_show::MAXIMIZED;
        if (w.minimized, w.maximized) == (minimized, maximized) {
            return Vec::new();
        }
        w.minimized = minimized;
        w.maximized = maximized;
        vec![ModelChange::StateChanged(m.window_id)]
    }

    fn on_icon(&mut self, m: WindowIcon) -> Vec<ModelChange> {
        let id = m.window_id;
        let Some(w) = self.windows.get_mut(&id) else {
            return Vec::new();
        };
        w.icon = Some(m);
        vec![ModelChange::IconChanged(id)]
    }

    /// Restack `window_id` to sit directly above `above_window_id` (0 = bottom).
    fn on_zorder(&mut self, window_id: u32, above_window_id: u32) -> Vec<ModelChange> {
        if !self.windows.contains_key(&window_id) {
            return Vec::new();
        }
        let Some(current) = self.stack.iter().position(|&s| s == window_id) else {
            return Vec::new();
        };

        let target = if above_window_id == 0 {
            0
        } else {
            match self.stack.iter().position(|&s| s == above_window_id) {
                // Insert after the reference window, accounting for the removal below.
                Some(i) if i > current => i,
                Some(i) => i + 1,
                // The reference window is unknown; leave the stack alone rather than guessing.
                None => return Vec::new(),
            }
        };
        if target == current {
            return Vec::new();
        }

        self.stack.remove(current);
        self.stack.insert(target.min(self.stack.len()), window_id);
        vec![ModelChange::Restacked]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxproto::message::{
        Close, CursorPosition, CursorVisibility, Error as ProtoError, WindowClosed, WindowZOrder,
    };

    fn opened(id: u32, title: &str) -> ClientEvent {
        ClientEvent::WindowOpened(WindowOpened {
            window_id: id,
            video_channel: 16 + id as u16,
            pid: 1,
            app_id: "notepad.exe".into(),
            title: title.into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            dpi: 96,
            flags: 0,
            owner_id: 0,
        })
    }

    #[test]
    fn opening_and_closing_a_window() {
        let mut m = WindowModel::new();
        assert_eq!(
            m.apply(opened(1, "Untitled")),
            vec![ModelChange::Created(1)]
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(1).unwrap().app_id, "notepad.exe");
        assert_eq!(m.stack(), &[1]);

        assert_eq!(
            m.apply(ClientEvent::WindowClosed(WindowClosed { window_id: 1 })),
            vec![ModelChange::Destroyed(1)]
        );
        assert!(m.is_empty());
        assert!(m.stack().is_empty());
    }

    #[test]
    fn window_flags_are_decoded_for_the_backend() {
        let mut m = WindowModel::new();
        let mut ev = opened(1, "Tooltip");
        if let ClientEvent::WindowOpened(ref mut w) = ev {
            // A frameless, non-resizable, always-on-top window — a tooltip or a menu.
            w.flags = window_flag::TOPMOST;
        }
        m.apply(ev);
        let w = m.get(1).unwrap();
        assert!(w.topmost);
        assert!(
            !w.resizable,
            "a backend must not offer to resize a fixed window"
        );
        assert!(
            !w.has_frame,
            "a backend must not decorate a frameless window"
        );

        let mut ev = opened(2, "Editor");
        if let ClientEvent::WindowOpened(ref mut w) = ev {
            w.flags = window_flag::RESIZABLE | window_flag::HAS_FRAME;
        }
        m.apply(ev);
        let w = m.get(2).unwrap();
        assert!(w.resizable && w.has_frame && !w.topmost);
    }

    #[test]
    fn a_duplicate_open_is_ignored() {
        let mut m = WindowModel::new();
        m.apply(opened(1, "a"));
        assert!(m.apply(opened(1, "b")).is_empty(), "no second Created");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(1).unwrap().title, "a", "the first mapping wins");
    }

    #[test]
    fn events_for_unknown_windows_are_ignored() {
        let mut m = WindowModel::new();
        // A geometry update racing with a close must not fabricate a window.
        assert!(m
            .apply(ClientEvent::WindowGeometry(WindowGeometry {
                window_id: 9,
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }))
            .is_empty());
        assert!(m
            .apply(ClientEvent::WindowClosed(WindowClosed { window_id: 9 }))
            .is_empty());
        assert!(m.is_empty());
    }

    #[test]
    fn only_real_changes_produce_work() {
        let mut m = WindowModel::new();
        m.apply(opened(1, "Untitled"));

        // Same geometry as it opened with: nothing for the backend to do.
        assert!(m
            .apply(ClientEvent::WindowGeometry(WindowGeometry {
                window_id: 1,
                x: 0,
                y: 0,
                width: 800,
                height: 600
            }))
            .is_empty());
        assert_eq!(
            m.apply(ClientEvent::WindowGeometry(WindowGeometry {
                window_id: 1,
                x: 10,
                y: 20,
                width: 800,
                height: 600
            })),
            vec![ModelChange::Moved(1)]
        );
        assert_eq!((m.get(1).unwrap().x, m.get(1).unwrap().y), (10, 20));

        assert!(m
            .apply(ClientEvent::WindowTitle(WindowTitle {
                window_id: 1,
                title: "Untitled".into()
            }))
            .is_empty());
        assert_eq!(
            m.apply(ClientEvent::WindowTitle(WindowTitle {
                window_id: 1,
                title: "Saved".into()
            })),
            vec![ModelChange::Retitled(1)]
        );
    }

    #[test]
    fn state_changes_track_minimize_and_maximize() {
        let mut m = WindowModel::new();
        m.apply(opened(1, "w"));
        assert_eq!(
            m.apply(ClientEvent::WindowState(WindowState {
                window_id: 1,
                state: window_show::MAXIMIZED,
                flags: 0
            })),
            vec![ModelChange::StateChanged(1)]
        );
        assert!(m.get(1).unwrap().maximized);
        assert!(!m.get(1).unwrap().minimized);
        // Repeating the same state is not a change.
        assert!(m
            .apply(ClientEvent::WindowState(WindowState {
                window_id: 1,
                state: window_show::MAXIMIZED,
                flags: 0
            }))
            .is_empty());
    }

    #[test]
    fn restacking_moves_a_window_above_another() {
        let mut m = WindowModel::new();
        m.apply(opened(1, "a"));
        m.apply(opened(2, "b"));
        m.apply(opened(3, "c"));
        assert_eq!(m.stack(), &[1, 2, 3]);

        // Put 1 directly above 2.
        assert_eq!(
            m.apply(ClientEvent::WindowZOrder(WindowZOrder {
                window_id: 1,
                above_window_id: 2
            })),
            vec![ModelChange::Restacked]
        );
        assert_eq!(m.stack(), &[2, 1, 3]);

        // Send 3 to the bottom.
        m.apply(ClientEvent::WindowZOrder(WindowZOrder {
            window_id: 3,
            above_window_id: 0,
        }));
        assert_eq!(m.stack(), &[3, 2, 1]);
    }

    #[test]
    fn restacking_against_an_unknown_reference_is_ignored() {
        let mut m = WindowModel::new();
        m.apply(opened(1, "a"));
        assert!(m
            .apply(ClientEvent::WindowZOrder(WindowZOrder {
                window_id: 1,
                above_window_id: 99
            }))
            .is_empty());
        assert_eq!(m.stack(), &[1]);
    }

    #[test]
    fn frames_pass_through_without_being_retained() {
        let mut m = WindowModel::new();
        m.apply(opened(1, "w"));
        let frame = FrameData {
            window_id: 1,
            frame_id: 1,
            codec: oxproto::codec::RAW_BGRA,
            flags: 0,
            width: 2,
            height: 1,
            captured_us: 1,
            encoded_us: 2,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let changes = m.apply(ClientEvent::Frame(frame.clone()));
        assert_eq!(changes, vec![ModelChange::Frame(frame)]);
        // The model holds metadata only — pixels are the renderer's business.
        assert_eq!(
            std::mem::size_of_val(m.get(1).unwrap()),
            size_of::<RemoteWindow>()
        );
    }

    #[test]
    fn cursor_and_session_events() {
        let mut m = WindowModel::new();
        assert!(m.cursor_visible());
        assert_eq!(
            m.apply(ClientEvent::CursorVisibility(CursorVisibility {
                visible: false
            })),
            vec![ModelChange::CursorVisibility(false)]
        );
        assert!(!m.cursor_visible());

        assert_eq!(
            m.apply(ClientEvent::CursorPosition(CursorPosition {
                window_id: 1,
                x: 5,
                y: 6
            })),
            vec![ModelChange::CursorMoved {
                window_id: 1,
                x: 5,
                y: 6
            }]
        );

        assert_eq!(
            m.apply(ClientEvent::Error(ProtoError {
                code: 6,
                message: "capture failed".into()
            })),
            vec![ModelChange::AgentError {
                code: 6,
                message: "capture failed".into()
            }]
        );
        assert_eq!(
            m.apply(ClientEvent::Closed(Close { reason: 0 })),
            vec![ModelChange::Closed]
        );
    }
}
