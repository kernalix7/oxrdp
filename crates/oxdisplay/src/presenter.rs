use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU32;
use std::time::Instant;

use oxproto::message::{codec, FrameData};
use raw_window_handle::{
    DisplayHandle, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle,
};
use softbuffer::{Context, Surface};

use crate::DisplayWindow;

/// Timing returned by a presenter after a frame is decoded and shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentTimes {
    /// Decode completion timestamp in client monotonic microseconds.
    pub decoded_us: u64,
    /// Presentation completion timestamp in client monotonic microseconds.
    pub presented_us: u64,
}

/// Error returned by a presenter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentError {
    /// The target window was not attached.
    UnknownWindow(u32),
    /// Frame codec is not supported by this presenter.
    UnsupportedCodec(u8),
    /// The frame payload length or dimensions are invalid.
    DroppedFrame {
        /// Window id.
        window_id: u32,
        /// Expected payload length.
        expected: usize,
        /// Actual payload length.
        actual: usize,
    },
    /// The platform presenter failed.
    Platform(String),
}

impl fmt::Display for PresentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownWindow(id) => write!(f, "unknown display window {id}"),
            Self::UnsupportedCodec(codec) => write!(f, "unsupported frame codec {codec}"),
            Self::DroppedFrame {
                window_id,
                expected,
                actual,
            } => write!(
                f,
                "dropped malformed frame for window {window_id}: expected {expected} bytes, got {actual}"
            ),
            Self::Platform(message) => f.write_str(message),
        }
    }
}

impl Error for PresentError {}

/// Puts pixels into an existing native window.
pub trait Presenter {
    /// Attach a presenter surface to a native window.
    fn attach(
        &mut self,
        id: u32,
        window: &dyn DisplayWindow,
        width: u32,
        height: u32,
    ) -> Result<(), PresentError>;

    /// Native surface size changed.
    fn resize(&mut self, id: u32, width: u32, height: u32) -> Result<(), PresentError>;

    /// Decode and present one frame.
    fn present(&mut self, id: u32, frame: &FrameData) -> Result<PresentTimes, PresentError>;

    /// Re-present the most recent frame for expose/redraw.
    fn refresh(&mut self, id: u32) -> Result<(), PresentError>;

    /// Detach and drop the native surface.
    fn detach(&mut self, id: u32);
}

/// CPU presenter for `RAW_BGRA` frames using `softbuffer`.
#[derive(Debug)]
pub struct CpuPresenter {
    windows: HashMap<u32, CpuWindow>,
    start: Instant,
}

impl CpuPresenter {
    /// Create an empty CPU presenter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            start: Instant::now(),
        }
    }

    fn now_us(&self) -> u64 {
        self.start
            .elapsed()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

impl Default for CpuPresenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Presenter for CpuPresenter {
    fn attach(
        &mut self,
        id: u32,
        window: &dyn DisplayWindow,
        width: u32,
        height: u32,
    ) -> Result<(), PresentError> {
        let handles = BorrowedHandles::from_window(window)?;
        let context = Context::new(handles).map_err(platform_error)?;
        let mut surface = Surface::new(&context, handles).map_err(platform_error)?;
        let width = nonzero(width);
        let height = nonzero(height);
        surface.resize(width, height).map_err(platform_error)?;
        self.windows.insert(
            id,
            CpuWindow {
                _context: context,
                surface,
                width: width.get(),
                height: height.get(),
                last_frame: None,
            },
        );
        Ok(())
    }

    fn resize(&mut self, id: u32, width: u32, height: u32) -> Result<(), PresentError> {
        let window = self
            .windows
            .get_mut(&id)
            .ok_or(PresentError::UnknownWindow(id))?;
        let width = nonzero(width);
        let height = nonzero(height);
        window
            .surface
            .resize(width, height)
            .map_err(platform_error)?;
        window.width = width.get();
        window.height = height.get();
        Ok(())
    }

    fn present(&mut self, id: u32, frame: &FrameData) -> Result<PresentTimes, PresentError> {
        if frame.codec != codec::RAW_BGRA {
            return Err(PresentError::UnsupportedCodec(frame.codec));
        }
        let expected = frame_len(frame.width, frame.height).ok_or(PresentError::DroppedFrame {
            window_id: frame.window_id,
            expected: usize::MAX,
            actual: frame.data.len(),
        })?;
        if frame.data.len() != expected {
            return Err(PresentError::DroppedFrame {
                window_id: frame.window_id,
                expected,
                actual: frame.data.len(),
            });
        }

        let decoded_us = self.now_us();
        let window = self
            .windows
            .get_mut(&id)
            .ok_or(PresentError::UnknownWindow(id))?;
        blit(window, frame)?;
        window.last_frame = Some(frame.clone());
        Ok(PresentTimes {
            decoded_us,
            presented_us: self.now_us(),
        })
    }

    fn refresh(&mut self, id: u32) -> Result<(), PresentError> {
        let frame = self
            .windows
            .get(&id)
            .ok_or(PresentError::UnknownWindow(id))?
            .last_frame
            .clone();
        if let Some(frame) = frame {
            let window = self
                .windows
                .get_mut(&id)
                .ok_or(PresentError::UnknownWindow(id))?;
            blit(window, &frame)?;
        }
        Ok(())
    }

    fn detach(&mut self, id: u32) {
        self.windows.remove(&id);
    }
}

#[derive(Debug)]
struct CpuWindow {
    _context: Context<BorrowedHandles>,
    surface: Surface<BorrowedHandles, BorrowedHandles>,
    width: u32,
    height: u32,
    last_frame: Option<FrameData>,
}

#[derive(Debug, Clone, Copy)]
struct BorrowedHandles {
    display: RawDisplayHandle,
    window: RawWindowHandle,
}

impl BorrowedHandles {
    fn from_window(window: &dyn DisplayWindow) -> Result<Self, PresentError> {
        Ok(Self {
            display: window.display_handle().map_err(platform_error)?.as_raw(),
            window: window.window_handle().map_err(platform_error)?.as_raw(),
        })
    }
}

impl HasDisplayHandle for BorrowedHandles {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
        // SAFETY: `BorrowedHandles` is created from a live `DisplayWindow` during
        // `Presenter::attach`. `oxdisplay` calls `Presenter::detach` and drops the surface before
        // the corresponding native window is destroyed, so the raw display handle remains valid
        // for every borrow returned here.
        Ok(unsafe { DisplayHandle::borrow_raw(self.display) })
    }
}

impl HasWindowHandle for BorrowedHandles {
    fn window_handle(&self) -> Result<WindowHandle<'_>, raw_window_handle::HandleError> {
        // SAFETY: Same attach-to-detach lifetime guarantee as `display_handle`; the native window
        // outlives this borrowed raw window handle.
        Ok(unsafe { WindowHandle::borrow_raw(self.window) })
    }
}

fn blit(window: &mut CpuWindow, frame: &FrameData) -> Result<(), PresentError> {
    let mut buffer = window.surface.buffer_mut().map_err(platform_error)?;
    let frame_width = u32::from(frame.width);
    let frame_height = u32::from(frame.height);
    let copy_width = window.width.min(frame_width);
    let copy_height = window.height.min(frame_height);

    if window.width != frame_width || window.height != frame_height {
        buffer.fill(0);
    }

    let dst_stride = usize::try_from(window.width).unwrap_or(usize::MAX) * 4;
    let src_stride = usize::from(frame.width) * 4;
    let row_bytes = usize::try_from(copy_width).unwrap_or(usize::MAX) * 4;
    let rows = usize::try_from(copy_height).unwrap_or(0);

    for row in 0..rows {
        let src_offset = row * src_stride;
        let dst_offset = row * dst_stride;
        // SAFETY: `frame.data` length was validated as `frame.width * frame.height * 4`.
        // `buffer` length is `window.width * window.height` u32s from softbuffer after resize.
        // `copy_width/copy_height` are min(frame, surface), so each row copy stays in bounds.
        // Copying as bytes also avoids imposing u32 alignment on the untrusted Vec<u8> payload.
        unsafe {
            std::ptr::copy_nonoverlapping(
                frame.data.as_ptr().add(src_offset),
                buffer.as_mut_ptr().cast::<u8>().add(dst_offset),
                row_bytes,
            );
        }
    }

    buffer.present().map_err(platform_error)
}

fn nonzero(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value.max(1)).expect("value is clamped to non-zero")
}

fn frame_len(width: u16, height: u16) -> Option<usize> {
    usize::from(width)
        .checked_mul(usize::from(height))?
        .checked_mul(4)
}

fn platform_error(error: impl fmt::Display) -> PresentError {
    PresentError::Platform(error.to_string())
}
