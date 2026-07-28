//! Display layer for the Linux oxproto client.
//!
//! This crate owns native window lifecycle and presentation for the first-pixels milestone:
//! `WindowModel` emits ordered `ModelChange`s, a display backend maps those changes to native
//! windows, and `CpuPresenter` blits `RAW_BGRA` frames into each window with `softbuffer`.
//!
//! Deviation record: `docs/design/client-display.md` says topmost/resizable flags map to native
//! window state, but `oxclient::RemoteWindow` does not retain those `WindowOpened.flags` fields
//! and this task forbids editing `oxclient::model`; this crate therefore cannot apply them from
//! `ModelChange`. `WindowZOrder` is intentionally ignored in v1, as specified by the decision
//! record.
//!
//! The CPU presenter contains the crate's only unsafe code. The public `Presenter` trait takes
//! `&dyn DisplayWindow`, matching the decision record, while `softbuffer::Surface` must store an
//! owned handle value. `CpuPresenter` snapshots raw handles and re-borrows them only for the
//! attach-to-detach interval that `oxdisplay` controls.
#![allow(unsafe_code)]

mod presenter;
mod winit_backend;

pub mod headless;
pub mod input;

use std::error::Error;
use std::fmt;

use oxclient::{ModelChange, RemoteWindow, WindowModel};
use oxproto::message::{
    CursorPosition, CursorShape, CursorVisibility, FrameData, Output, WindowClosed, WindowGeometry,
    WindowIcon, WindowOpened, WindowState, WindowTitle, WindowZOrder,
};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use x11rb::connection::Connection;

pub use presenter::{CpuPresenter, PresentError, PresentTimes, Presenter};
pub use winit_backend::{run, Closed, CommandSender};

/// Anything a presenter can bind a surface to.
///
/// `oxdisplay` guarantees the native handle outlives the presenter's attach-to-detach interval.
pub trait DisplayWindow: HasWindowHandle + HasDisplayHandle {}

impl<T> DisplayWindow for T where T: HasWindowHandle + HasDisplayHandle {}

/// Session thread to display thread command.
///
/// The variants mirror the display-relevant protocol events. The display loop forwards them
/// through `WindowModel`; it does not re-diff protocol messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayCommand {
    /// Create a native window.
    OpenWindow(WindowOpened),
    /// Apply guest geometry.
    Geometry(WindowGeometry),
    /// Apply a window title.
    Title(WindowTitle),
    /// Apply minimized/maximized/restored state.
    State(WindowState),
    /// Accept z-order update; ignored in v1 after model ordering is updated.
    ZOrder(WindowZOrder),
    /// Apply a window icon where the platform supports it.
    Icon(WindowIcon),
    /// Destroy a native window.
    CloseWindow(WindowClosed),
    /// Present a frame.
    Frame(FrameData),
    /// Set cursor bitmap.
    CursorShape(CursorShape),
    /// Accepted for protocol forward compatibility; ignored in v1.
    CursorPosition(CursorPosition),
    /// Show or hide native cursor.
    CursorVisibility(CursorVisibility),
    /// Stop the display loop.
    Shutdown,
}

/// Display thread to session thread event.
///
/// Coordinates are physical pixels. Pointer coordinates are window-relative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayEvent {
    /// Pointer motion, button, or wheel state.
    Pointer {
        /// Target window.
        window_id: u32,
        /// Window-relative X.
        x: i32,
        /// Window-relative Y.
        y: i32,
        /// Held button bitmask.
        buttons: u8,
        /// Horizontal wheel delta in protocol units.
        wheel_x: i16,
        /// Vertical wheel delta in protocol units.
        wheel_y: i16,
    },
    /// Keyboard key press or release, already translated to PS/2 set 1.
    Key {
        /// PS/2 set 1 scancode.
        scancode: u16,
        /// `true` on press, `false` on release.
        pressed: bool,
        /// Whether the scancode has an E0 prefix.
        extended: bool,
    },
    /// UTF-8 text input for the IME path.
    Text {
        /// Text to send.
        text: String,
    },
    /// Authoritative modifier and lock state.
    Modifiers {
        /// Protocol modifier bitmask.
        modifiers: u16,
        /// Protocol lock bitmask.
        locks: u8,
    },
    /// Native focus changed.
    Focused {
        /// Window id.
        window_id: u32,
        /// Whether focus was gained.
        focused: bool,
    },
    /// User requested window close.
    CloseRequested {
        /// Window id.
        window_id: u32,
    },
    /// Local window size changed.
    ResizeRequested {
        /// Window id.
        window_id: u32,
        /// New width.
        width: u16,
        /// New height.
        height: u16,
    },
    /// Local X11 window position changed.
    MoveRequested {
        /// Window id.
        window_id: u32,
        /// New X.
        x: i32,
        /// New Y.
        y: i32,
    },
    /// Minimized state changed.
    Minimized {
        /// Window id.
        window_id: u32,
        /// Whether minimized.
        minimized: bool,
    },
    /// Frame was presented.
    Presented {
        /// Window id.
        window_id: u32,
        /// Frame id.
        frame_id: u64,
        /// Decode completion timestamp.
        decoded_us: u64,
        /// Presentation completion timestamp.
        presented_us: u64,
    },
    /// Fatal backend error.
    BackendError {
        /// Human-readable error.
        message: String,
    },
}

/// Error returned when the display loop or backend fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayError {
    message: String,
}

impl DisplayError {
    /// Creates a display error with a human-readable message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Error text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for DisplayError {}

/// A backend that executes already-ordered `WindowModel` changes.
///
/// This trait is intentionally smaller than the winit event loop. It lets the headless backend
/// test the model-to-backend mapping without a display server.
pub trait DisplayBackend {
    /// Create one native window.
    fn create_window(&mut self, window: &RemoteWindow) -> Result<(), DisplayError>;
    /// Destroy one native window.
    fn destroy_window(&mut self, window_id: u32) -> Result<(), DisplayError>;
    /// Apply geometry; X11 backends also apply position, Wayland backends only apply size.
    fn move_window(&mut self, window: &RemoteWindow) -> Result<(), DisplayError>;
    /// Apply title.
    fn retitle_window(&mut self, window: &RemoteWindow) -> Result<(), DisplayError>;
    /// Apply minimized/maximized/restored state.
    fn change_state(&mut self, window: &RemoteWindow) -> Result<(), DisplayError>;
    /// Apply icon if supported.
    fn change_icon(&mut self, window: &RemoteWindow) -> Result<(), DisplayError>;
    /// Accept restack notification. V1 ignores this after `WindowModel` updates ordering.
    fn restack(&mut self, stack: &[u32]) -> Result<(), DisplayError>;
    /// Present a frame.
    fn frame(&mut self, frame: &FrameData) -> Result<Option<PresentTimes>, DisplayError>;
    /// Apply cursor shape.
    fn cursor_shape(&mut self, shape: &CursorShape) -> Result<(), DisplayError>;
    /// Accept cursor move. V1 ignores it.
    fn cursor_moved(&mut self, window_id: u32, x: i32, y: i32) -> Result<(), DisplayError>;
    /// Apply cursor visibility.
    fn cursor_visibility(&mut self, visible: bool) -> Result<(), DisplayError>;
    /// Surface an agent error.
    fn agent_error(&mut self, code: u16, message: &str) -> Result<(), DisplayError>;
    /// Close the backend.
    fn closed(&mut self) -> Result<(), DisplayError>;
}

/// Applies one model change to a backend.
///
/// Unknown windows are ignored because `WindowModel` can legally emit frame races against close.
pub fn apply_model_change<B: DisplayBackend + ?Sized>(
    backend: &mut B,
    model: &WindowModel,
    change: ModelChange,
) -> Result<(), DisplayError> {
    match change {
        ModelChange::Created(id) => {
            if let Some(window) = model.get(id) {
                backend.create_window(window)?;
            }
        }
        ModelChange::Destroyed(id) => backend.destroy_window(id)?,
        ModelChange::Moved(id) => {
            if let Some(window) = model.get(id) {
                backend.move_window(window)?;
            }
        }
        ModelChange::Retitled(id) => {
            if let Some(window) = model.get(id) {
                backend.retitle_window(window)?;
            }
        }
        ModelChange::StateChanged(id) => {
            if let Some(window) = model.get(id) {
                backend.change_state(window)?;
            }
        }
        ModelChange::IconChanged(id) => {
            if let Some(window) = model.get(id) {
                backend.change_icon(window)?;
            }
        }
        ModelChange::Restacked => backend.restack(model.stack())?,
        ModelChange::Frame(frame) => {
            let _ = backend.frame(&frame)?;
        }
        ModelChange::CursorShape(shape) => backend.cursor_shape(&shape)?,
        ModelChange::CursorMoved { window_id, x, y } => {
            backend.cursor_moved(window_id, x, y)?;
        }
        ModelChange::CursorVisibility(visible) => backend.cursor_visibility(visible)?,
        ModelChange::AgentError { code, message } => backend.agent_error(code, &message)?,
        ModelChange::Closed => backend.closed()?,
    }
    Ok(())
}

/// Enumerates outputs for the session display layout.
///
/// This uses an independent X11 connection when available so it cannot consume winit's single
/// process event loop before `run`. Headless and Wayland-only startup paths return an empty list;
/// later Wayland output enumeration belongs with the Wayland-backend milestone.
#[must_use]
pub fn outputs() -> Vec<Output> {
    let Ok((conn, _screen_num)) = x11rb::connect(None) else {
        return Vec::new();
    };
    conn.setup()
        .roots
        .iter()
        .enumerate()
        .filter_map(|(index, screen)| {
            Some(Output {
                id: u8::try_from(index).ok()?,
                x: 0,
                y: 0,
                width: screen.width_in_pixels,
                height: screen.height_in_pixels,
                scale_num: 1,
                scale_den: 1,
                refresh_mhz: 60_000,
            })
        })
        .collect()
}

pub(crate) fn display_error(error: impl fmt::Display) -> DisplayError {
    DisplayError::new(error.to_string())
}
