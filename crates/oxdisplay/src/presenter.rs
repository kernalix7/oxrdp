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

    /// Present one frame.
    ///
    /// Frames arrive already decoded: `oxclient::decode` turns whatever codec was negotiated
    /// into `RAW_BGRA` before the frame crosses to this thread, which is what keeps this layer
    /// codec-agnostic and keeps [`CpuPresenter`] a `memcpy`. A frame in any other codec is a bug
    /// upstream, not something to decode here.
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
        window.cache_frame(frame);
        Ok(PresentTimes {
            decoded_us,
            presented_us: self.now_us(),
        })
    }

    fn refresh(&mut self, id: u32) -> Result<(), PresentError> {
        let window = self
            .windows
            .get_mut(&id)
            .ok_or(PresentError::UnknownWindow(id))?;
        if let Some(frame) = window.last_frame.as_ref() {
            blit_pixels(
                &mut window.surface,
                window.width,
                window.height,
                u32::from(frame.width),
                u32::from(frame.height),
                &frame.data,
            )?;
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
    last_frame: Option<CachedFrame>,
}

impl CpuWindow {
    fn cache_frame(&mut self, frame: &FrameData) {
        match self.last_frame.as_mut() {
            Some(cached) if cached.data.capacity() >= frame.data.len() => {
                cached.replace_from(frame);
            }
            _ => {
                self.last_frame = Some(CachedFrame::from(frame));
            }
        }
    }
}

#[derive(Debug)]
struct CachedFrame {
    window_id: u32,
    frame_id: u64,
    codec: u8,
    flags: u8,
    width: u16,
    height: u16,
    captured_us: u64,
    encoded_us: u64,
    data: Vec<u8>,
}

impl From<&FrameData> for CachedFrame {
    fn from(frame: &FrameData) -> Self {
        Self {
            window_id: frame.window_id,
            frame_id: frame.frame_id,
            codec: frame.codec,
            flags: frame.flags,
            width: frame.width,
            height: frame.height,
            captured_us: frame.captured_us,
            encoded_us: frame.encoded_us,
            data: frame.data.clone(),
        }
    }
}

impl CachedFrame {
    fn replace_from(&mut self, frame: &FrameData) {
        self.window_id = frame.window_id;
        self.frame_id = frame.frame_id;
        self.codec = frame.codec;
        self.flags = frame.flags;
        self.width = frame.width;
        self.height = frame.height;
        self.captured_us = frame.captured_us;
        self.encoded_us = frame.encoded_us;
        self.data.clear();
        self.data.extend_from_slice(&frame.data);
    }
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
    blit_pixels(
        &mut window.surface,
        window.width,
        window.height,
        u32::from(frame.width),
        u32::from(frame.height),
        &frame.data,
    )
}

fn blit_pixels(
    surface: &mut Surface<BorrowedHandles, BorrowedHandles>,
    window_width: u32,
    window_height: u32,
    frame_width: u32,
    frame_height: u32,
    frame_data: &[u8],
) -> Result<(), PresentError> {
    let mut buffer = surface.buffer_mut().map_err(platform_error)?;
    let copy_width = window_width.min(frame_width);
    let copy_height = window_height.min(frame_height);

    if window_width != frame_width || window_height != frame_height {
        buffer.fill(0);
    }

    let dst_stride = usize::try_from(window_width).unwrap_or(usize::MAX) * 4;
    let src_stride = usize::try_from(frame_width).unwrap_or(usize::MAX) * 4;
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
                frame_data.as_ptr().add(src_offset),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_len_counts_bgra_bytes() {
        assert_eq!(frame_len(0, 10), Some(0));
        assert_eq!(frame_len(800, 600), Some(1_920_000));
        assert_eq!(frame_len(u16::MAX, u16::MAX), Some(17_179_344_900));
    }

    #[test]
    fn cached_frame_can_reuse_existing_capacity() {
        let first = FrameData {
            window_id: 1,
            frame_id: 1,
            codec: codec::RAW_BGRA,
            flags: 0,
            width: 2,
            height: 1,
            captured_us: 10,
            encoded_us: 11,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let second = FrameData {
            frame_id: 2,
            captured_us: 12,
            encoded_us: 13,
            data: vec![9, 10, 11, 12],
            ..first.clone()
        };
        let mut cached = CachedFrame::from(&first);
        let capacity = cached.data.capacity();

        cached.replace_from(&second);

        assert_eq!(cached.frame_id, 2);
        assert_eq!(cached.data, second.data);
        assert_eq!(cached.data.capacity(), capacity);
    }
}
