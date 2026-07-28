use std::collections::HashSet;

use oxproto::message::{CursorShape, FrameData};

use crate::{DisplayBackend, DisplayError, PresentTimes, WindowSpec};

/// Recorded backend call for headless tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessCall {
    /// Create a window.
    Created {
        /// Window id.
        window_id: u32,
        /// Application id.
        app_id: String,
        /// Window title.
        title: String,
        /// X coordinate.
        x: i32,
        /// Y coordinate.
        y: i32,
        /// Width.
        width: u16,
        /// Height.
        height: u16,
        /// Owner window id.
        owner_id: u32,
    },
    /// Destroy a window.
    Destroyed(u32),
    /// Move or resize a window.
    Moved {
        /// Window id.
        window_id: u32,
        /// X coordinate.
        x: i32,
        /// Y coordinate.
        y: i32,
        /// Width.
        width: u16,
        /// Height.
        height: u16,
    },
    /// Retitle a window.
    Retitled {
        /// Window id.
        window_id: u32,
        /// New title.
        title: String,
    },
    /// Change state.
    StateChanged {
        /// Window id.
        window_id: u32,
        /// Minimized.
        minimized: bool,
        /// Maximized.
        maximized: bool,
    },
    /// Change icon.
    IconChanged(u32),
    /// Restack windows bottom-to-top.
    Restacked(Vec<u32>),
    /// Present frame.
    Frame {
        /// Window id.
        window_id: u32,
        /// Frame id.
        frame_id: u64,
        /// Payload bytes.
        bytes: usize,
    },
    /// Cursor shape changed.
    CursorShape {
        /// Cursor id.
        cursor_id: u32,
        /// Width.
        width: u16,
        /// Height.
        height: u16,
    },
    /// Cursor moved.
    CursorMoved {
        /// Window id.
        window_id: u32,
        /// X.
        x: i32,
        /// Y.
        y: i32,
    },
    /// Cursor visibility changed.
    CursorVisibility(bool),
    /// Agent error surfaced.
    AgentError {
        /// Error code.
        code: u16,
        /// Error message.
        message: String,
    },
    /// Session closed.
    Closed,
}

/// Headless backend that records calls without opening native windows.
#[derive(Debug, Default)]
pub struct HeadlessBackend {
    calls: Vec<HeadlessCall>,
    windows: HashSet<u32>,
}

impl HeadlessBackend {
    /// Create an empty headless backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Recorded calls.
    #[must_use]
    pub fn calls(&self) -> &[HeadlessCall] {
        &self.calls
    }

    /// Remove and return recorded calls.
    pub fn take_calls(&mut self) -> Vec<HeadlessCall> {
        std::mem::take(&mut self.calls)
    }
}

impl DisplayBackend for HeadlessBackend {
    fn create_window(&mut self, window: &WindowSpec) -> Result<(), DisplayError> {
        self.windows.insert(window.window_id);
        self.calls.push(HeadlessCall::Created {
            window_id: window.window_id,
            app_id: window.app_id.clone(),
            title: window.title.clone(),
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
            owner_id: window.owner_id,
        });
        Ok(())
    }

    fn destroy_window(&mut self, window_id: u32) -> Result<(), DisplayError> {
        self.windows.remove(&window_id);
        self.calls.push(HeadlessCall::Destroyed(window_id));
        Ok(())
    }

    fn move_window(&mut self, window: &WindowSpec) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::Moved {
            window_id: window.window_id,
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        });
        Ok(())
    }

    fn retitle_window(&mut self, window: &WindowSpec) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::Retitled {
            window_id: window.window_id,
            title: window.title.clone(),
        });
        Ok(())
    }

    fn change_state(&mut self, window: &WindowSpec) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::StateChanged {
            window_id: window.window_id,
            minimized: window.minimized,
            maximized: window.maximized,
        });
        Ok(())
    }

    fn change_icon(&mut self, window: &WindowSpec) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::IconChanged(window.window_id));
        Ok(())
    }

    fn restack(&mut self, stack: &[u32]) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::Restacked(stack.to_vec()));
        Ok(())
    }

    fn frame(&mut self, frame: &FrameData) -> Result<Option<PresentTimes>, DisplayError> {
        if !self.windows.contains(&frame.window_id) {
            return Ok(None);
        }
        self.calls.push(HeadlessCall::Frame {
            window_id: frame.window_id,
            frame_id: frame.frame_id,
            bytes: frame.data.len(),
        });
        Ok(Some(PresentTimes {
            decoded_us: 0,
            presented_us: 0,
        }))
    }

    fn cursor_shape(&mut self, shape: &CursorShape) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::CursorShape {
            cursor_id: shape.cursor_id,
            width: shape.width,
            height: shape.height,
        });
        Ok(())
    }

    fn cursor_moved(&mut self, window_id: u32, x: i32, y: i32) -> Result<(), DisplayError> {
        self.calls
            .push(HeadlessCall::CursorMoved { window_id, x, y });
        Ok(())
    }

    fn cursor_visibility(&mut self, visible: bool) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::CursorVisibility(visible));
        Ok(())
    }

    fn agent_error(&mut self, code: u16, message: &str) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::AgentError {
            code,
            message: message.to_owned(),
        });
        Ok(())
    }

    fn closed(&mut self) -> Result<(), DisplayError> {
        self.calls.push(HeadlessCall::Closed);
        Ok(())
    }
}
