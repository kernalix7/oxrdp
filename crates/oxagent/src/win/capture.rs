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

use windows::core::{Interface, Result as WinResult};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
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
}

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
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalizedSrgb,
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
        let Ok(frame) = self.frame_pool.TryGetNextFrame() else {
            return Ok(None);
        };

        // A resized window changes ContentSize; the pool must be rebuilt or every later
        // frame is captured at the stale size.
        let content = frame.ContentSize()?;
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            self.frame_pool.Recreate(
                &self.d3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalizedSrgb,
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
        // SAFETY: both resources are valid and have identical descriptions apart from usage.
        unsafe { self.context.CopyResource(&staging, &texture) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging texture is CPU-readable; unmapped below on every path.
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };

        let row_bytes = desc.Width as usize * 4;
        let mut bgra = vec![0u8; row_bytes * desc.Height as usize];
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
