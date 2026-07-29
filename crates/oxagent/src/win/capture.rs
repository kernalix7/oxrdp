//! Per-window capture via Windows.Graphics.Capture (WGC).
//!
//! One [`WindowCapture`] owns a D3D11 device, a capture item bound to a single `HWND`, a
//! free-threaded frame pool, and a reused staging texture. [`WindowCapture::try_next_frame`]
//! is non-blocking: it returns `Ok(None)` when the pool has no new frame.
//!
//! Deliberate choices (each one is a real WGC pitfall):
//! - **Free-threaded frame pool.** `Direct3D11CaptureFramePool::Create` requires a
//!   `DispatcherQueue` on the calling thread; `CreateFreeThreaded` does not, so the capture
//!   loop can be a plain worker thread with no message pump.
//! - **Cursor is not baked in.** `SetIsCursorCaptureEnabled(false)`: the cursor travels as its
//!   own protocol message so the client can render it at input latency instead of frame
//!   latency.
//! - **No capture border.** `SetIsBorderRequired(false)` where the OS build supports it (it is
//!   best-effort; older builds reject it).
//! - **Staging texture is reused** and only recreated when the size changes — allocating one
//!   per frame is the classic WGC performance mistake.
//! - **Row-pitch aware readback.** `RowPitch` is not `width * 4`; copying naively produces a
//!   sheared image.
//! - Each `Direct3D11CaptureFrame` is dropped at the end of the iteration; holding frames
//!   starves the pool.
//! - **The readback is timed and split, permanently.** A guest latency measurement found
//!   `capture->encode` costing roughly 6ms more than the H.264 encoder's own colour conversion
//!   and compute (`crate::win::encode`) could account for, with nobody having looked at capture
//!   itself. `try_next_frame` times the frame-pool acquire, the GPU-side copy, the CPU-blocking
//!   `Map`, and the actual row-by-row memcpy separately, for the first `DIAGNOSTIC_FRAME_LIMIT`
//!   frames of every window — the leading suspect going in was `Map`, which is where a staging
//!   texture readback actually pays the cost of crossing the PCIe boundary on a virtualised GPU,
//!   but the log says what it says rather than what was expected, the same way this project's
//!   last two latency guesses both turned out to need correcting by the data.

use windows::core::{Interface, Result as WinResult};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_POINTER, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// Number of buffers in the capture frame pool. Two is the low-latency choice: enough to
/// avoid stalling the compositor, few enough that a frame cannot sit queued for long.
const FRAME_POOL_BUFFERS: i32 = 2;

/// A captured frame as tightly packed BGRA (stride is exactly `width * 4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Tightly packed BGRA8 pixels, top-down.
    pub bgra: Vec<u8>,
}

/// Whether this guest supports Windows.Graphics.Capture at all (Windows 10 1903+).
pub fn is_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// A live capture of a single window.
pub struct WindowCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Kept alive for the lifetime of the session and queried by [`WindowCapture::size`].
    #[allow(dead_code)]
    item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    d3d_device: IDirect3DDevice,
    /// Reused CPU-readable texture, recreated only when the captured size changes.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Size the frame pool is currently configured for.
    pool_size: SizeInt32,
    /// Native handle, kept only for `DIAGNOSTIC_FRAME_LIMIT` logging.
    handle: isize,
    /// How many frames this instance has captured so far, counted only up to
    /// `DIAGNOSTIC_FRAME_LIMIT`.
    frames_seen: u32,
}

/// How many frames, per [`WindowCapture`], to log the readback timing split of — the same
/// bounded, permanent-diagnostic shape as `crate::win::encode`'s constant of the same name, and
/// `crate::serve`'s `CAPTURE_DIAGNOSTIC_FRAME_LIMIT`, kept local rather than shared since each
/// lives on a different side of a Windows/platform-independent boundary this crate otherwise
/// keeps strict.
const DIAGNOSTIC_FRAME_LIMIT: u32 = 100;

impl WindowCapture {
    /// Begin capturing `hwnd`.
    pub fn new(hwnd: HWND) -> WinResult<Self> {
        let (device, context) = create_d3d_device()?;
        let d3d_device = winrt_device(&device)?;

        // A capture item is created from an HWND through the Win32 interop factory.
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: `hwnd` is a valid top-level window handle supplied by the caller.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };

        let pool_size = item.Size()?;
        // `Direct3D11CaptureFramePool` accepts exactly two pixel formats:
        // `B8G8R8A8UIntNormalized` and `R16G16B16A16Float`. The *sRGB* BGRA variant looks like
        // the obvious choice and compiles fine, but the pool rejects it at runtime with a bare
        // `E_INVALIDARG` — which is indistinguishable from a bad HWND and cost a debugging
        // session to pin down. Do not "restore" the sRGB form here.
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            FRAME_POOL_BUFFERS,
            pool_size,
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;
        // The cursor is streamed separately; never bake it into the frame.
        let _ = session.SetIsCursorCaptureEnabled(false);
        // Best-effort: not available on every Windows build.
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;

        Ok(Self {
            device,
            context,
            item,
            frame_pool,
            _session: session,
            d3d_device,
            staging: None,
            pool_size,
            handle: hwnd.0 as isize,
            frames_seen: 0,
        })
    }

    /// The window's current capture size, as reported by the capture item.
    ///
    /// Used to detect a resize before a frame arrives, so the driver can re-announce geometry
    /// without waiting for the frame pool to be recreated.
    #[allow(dead_code)]
    pub fn size(&self) -> WinResult<SizeInt32> {
        self.item.Size()
    }

    /// Try to take the next frame. Returns `Ok(None)` when no new frame is queued.
    pub fn try_next_frame(&mut self) -> WinResult<Option<Frame>> {
        // An empty pool is not an error, but windows-rs still reports it as one: the WinRT call
        // succeeds with `S_OK` and hands back a *null* frame, and the generated binding turns any
        // null return into `Err` — carrying the original, successful `S_OK` (some builds use
        // `E_POINTER`). So the empty-pool sentinel is an `Err` whose HRESULT is not a failure.
        //
        // Both halves of this matter. Treating every error as an empty pool hides a genuinely
        // broken capture as an idle window — a silent stall with nothing logged. Treating the
        // empty pool as an error is worse: the caller drops the capture and rebuilds it on the
        // next tick, so the pool is destroyed before it can ever fill, and the stream produces
        // exactly zero frames forever while looking busy.
        let pool_acquire_start = std::time::Instant::now();
        let frame = match self.frame_pool.TryGetNextFrame() {
            Ok(frame) => frame,
            Err(e) if e.code().is_ok() || e.code() == E_POINTER => return Ok(None),
            Err(e) => return Err(e),
        };
        let pool_acquire_us = pool_acquire_start.elapsed().as_micros() as u64;

        // A resized window changes ContentSize; the pool must be rebuilt or every later
        // frame is captured at the stale size.
        let content = frame.ContentSize()?;
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            self.frame_pool.Recreate(
                &self.d3d_device,
                // Must match the format the pool was created with — see the note there.
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                FRAME_POOL_BUFFERS,
                content,
            )?;
            self.pool_size = content;
            // This frame is still the old size; skip it and let the caller poll again.
            return Ok(None);
        }

        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        // SAFETY: the surface is a D3D11 surface, so the underlying interface is a texture.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` is valid and `desc` is a valid out-pointer.
        unsafe { texture.GetDesc(&mut desc) };

        let staging = self.staging_texture(&desc)?;
        let copy_start = std::time::Instant::now();
        // SAFETY: both resources are valid and have identical descriptions apart from usage.
        unsafe { self.context.CopyResource(&staging, &texture) };
        let copy_resource_us = copy_start.elapsed().as_micros() as u64;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let map_start = std::time::Instant::now();
        // SAFETY: staging texture is CPU-readable; unmapped below on every path.
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        // `Map` on a staging texture is where a GPU→CPU readback actually pays for itself: it
        // has to wait for `CopyResource`'s GPU-side work to finish before the mapping can be
        // handed back, which on a virtualised GPU means a PCIe-boundary DMA wait `CopyResource`
        // issuing the copy does not itself incur. Timed separately from `CopyResource` for
        // exactly that reason — the two calls have very different cost profiles even though
        // they are both "the readback" from outside this function.
        let map_us = map_start.elapsed().as_micros() as u64;

        let row_bytes = desc.Width as usize * 4;
        let mut bgra = vec![0u8; row_bytes * desc.Height as usize];
        let readback_copy_start = std::time::Instant::now();
        // SAFETY: `pData` points to at least `RowPitch * Height` bytes; we copy exactly
        // `row_bytes` per row, which is <= RowPitch.
        unsafe {
            let src = mapped.pData as *const u8;
            for y in 0..desc.Height as usize {
                std::ptr::copy_nonoverlapping(
                    src.add(y * mapped.RowPitch as usize),
                    bgra.as_mut_ptr().add(y * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(&staging, 0);
        }
        let readback_copy_us = readback_copy_start.elapsed().as_micros() as u64;

        if self.frames_seen < DIAGNOSTIC_FRAME_LIMIT {
            self.frames_seen += 1;
            eprintln!(
                "oxagent: capture: window={:#x} frame={} pool_acquire_us={pool_acquire_us} \
                 copy_resource_us={copy_resource_us} map_us={map_us} \
                 readback_copy_us={readback_copy_us} total_capture_us={}",
                self.handle,
                self.frames_seen,
                pool_acquire_us + copy_resource_us + map_us + readback_copy_us
            );
        }

        Ok(Some(Frame {
            width: desc.Width,
            height: desc.Height,
            bgra,
        }))
    }

    /// The reused staging texture, recreated only when the frame size changes.
    fn staging_texture(&mut self, src: &D3D11_TEXTURE2D_DESC) -> WinResult<ID3D11Texture2D> {
        if let Some((tex, w, h)) = &self.staging {
            if *w == src.Width && *h == src.Height {
                return Ok(tex.clone());
            }
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            MipLevels: 1,
            ArraySize: 1,
            ..*src
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` is fully initialized; `tex` is a valid out-parameter.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.expect("CreateTexture2D succeeded without producing a texture");
        self.staging = Some((tex.clone(), src.Width, src.Height));
        Ok(tex)
    }
}

/// Create a D3D11 device, preferring the GPU and falling back to WARP (software) so the
/// agent still runs on a guest without GPU acceleration.
fn create_d3d_device() -> WinResult<(ID3D11Device, ID3D11DeviceContext)> {
    let mut last = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: out-parameters are valid; no adapter/software module is supplied.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                // BGRA support is required for interop with WinRT/Direct2D surfaces.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match result {
            Ok(()) => {
                if let (Some(device), Some(context)) = (device, context) {
                    return Ok((device, context));
                }
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(windows::core::Error::from_win32))
}

/// Bridge a Win32 `ID3D11Device` into the WinRT `IDirect3DDevice` that WGC requires.
fn winrt_device(device: &ID3D11Device) -> WinResult<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: `dxgi` is a valid DXGI device.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    inspectable.cast()
}
