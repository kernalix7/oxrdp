use std::collections::HashMap;
use std::sync::Arc;

use oxclient::{ClientEvent, RemoteWindow, WindowModel};
use oxproto::message::input::cursor_format;
use oxproto::message::{CursorShape, FrameData};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::platform::x11::{ActiveEventLoopExtX11, WindowAttributesExtX11};
use winit::window::{CustomCursor, Icon, Window, WindowId, WindowLevel};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, PropMode};
use x11rb::wrapper::ConnectionExt as _;

use crate::input::{key_flags, keycode_to_scancode, locks, modifiers, update_buttons};
use crate::{
    apply_model_change, display_error, DisplayBackend, DisplayCommand, DisplayError, DisplayEvent,
    PresentError, PresentTimes, Presenter,
};

/// Cloneable command sender for the display event loop.
#[derive(Debug, Clone)]
pub struct CommandSender {
    proxy: EventLoopProxy<DisplayCommand>,
}

impl CommandSender {
    /// Send a command and wake the display event loop.
    pub fn send(&self, cmd: DisplayCommand) -> Result<(), Closed> {
        self.proxy.send_event(cmd).map_err(|_| Closed)
    }
}

/// The display event loop is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// Runs the platform event loop on the calling thread.
pub fn run(
    presenter: Box<dyn Presenter>,
    events: mpsc::UnboundedSender<DisplayEvent>,
    ready: impl FnOnce(CommandSender),
) -> Result<(), DisplayError> {
    let event_loop = EventLoop::<DisplayCommand>::with_user_event()
        .build()
        .map_err(display_error)?;
    let proxy = event_loop.create_proxy();
    ready(CommandSender { proxy });

    let mut app = DisplayApp {
        model: WindowModel::new(),
        backend: WinitBackend::new(presenter, events),
    };
    event_loop.run_app(&mut app).map_err(display_error)
}

struct DisplayApp {
    model: WindowModel,
    backend: WinitBackend,
}

impl ApplicationHandler<DisplayCommand> for DisplayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: DisplayCommand) {
        if matches!(event, DisplayCommand::Shutdown) {
            event_loop.exit();
            return;
        }

        let Some(client_event) = command_to_client_event(event) else {
            return;
        };
        for change in self.model.apply(client_event) {
            let mut backend = WinitBackendAdapter {
                inner: &mut self.backend,
                event_loop,
            };
            if let Err(error) = apply_model_change(&mut backend, &self.model, change) {
                let _ = self.backend.events.send(DisplayEvent::BackendError {
                    message: error.to_string(),
                });
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.backend.handle_window_event(event_loop, id, event);
    }
}

fn command_to_client_event(command: DisplayCommand) -> Option<ClientEvent> {
    Some(match command {
        DisplayCommand::OpenWindow(m) => ClientEvent::WindowOpened(m),
        DisplayCommand::Geometry(m) => ClientEvent::WindowGeometry(m),
        DisplayCommand::Title(m) => ClientEvent::WindowTitle(m),
        DisplayCommand::State(m) => ClientEvent::WindowState(m),
        DisplayCommand::ZOrder(m) => ClientEvent::WindowZOrder(m),
        DisplayCommand::Icon(m) => ClientEvent::WindowIcon(m),
        DisplayCommand::CloseWindow(m) => ClientEvent::WindowClosed(m),
        DisplayCommand::Frame(m) => ClientEvent::Frame(m),
        DisplayCommand::CursorShape(m) => ClientEvent::CursorShape(m),
        DisplayCommand::CursorPosition(m) => ClientEvent::CursorPosition(m),
        DisplayCommand::CursorVisibility(m) => ClientEvent::CursorVisibility(m),
        DisplayCommand::Shutdown => return None,
    })
}

struct WinitBackend {
    presenter: Box<dyn Presenter>,
    events: mpsc::UnboundedSender<DisplayEvent>,
    windows: HashMap<u32, WinitWindow>,
    by_winit_id: HashMap<WindowId, u32>,
    cursor_cache: HashMap<u32, winit::window::CustomCursor>,
    cursor_visible: bool,
    pointer_buttons: u8,
    pointer_position: HashMap<u32, (i32, i32)>,
    x11_sidecar: X11Sidecar,
}

impl WinitBackend {
    fn new(presenter: Box<dyn Presenter>, events: mpsc::UnboundedSender<DisplayEvent>) -> Self {
        Self {
            presenter,
            events,
            windows: HashMap::new(),
            by_winit_id: HashMap::new(),
            cursor_cache: HashMap::new(),
            cursor_visible: true,
            pointer_buttons: 0,
            pointer_position: HashMap::new(),
            x11_sidecar: X11Sidecar::new(),
        }
    }

    fn handle_window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window_id) = self.by_winit_id.get(&id).copied() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.events.send(DisplayEvent::CloseRequested { window_id });
            }
            WindowEvent::Resized(size) => {
                let width = u16::try_from(size.width).unwrap_or(u16::MAX);
                let height = u16::try_from(size.height).unwrap_or(u16::MAX);
                let _ = self
                    .presenter
                    .resize(window_id, u32::from(width), u32::from(height));
                let _ = self.events.send(DisplayEvent::ResizeRequested {
                    window_id,
                    width,
                    height,
                });
            }
            WindowEvent::Moved(position) if event_loop.is_x11() => {
                let _ = self.events.send(DisplayEvent::MoveRequested {
                    window_id,
                    x: position.x,
                    y: position.y,
                });
            }
            WindowEvent::Moved(_) => {}
            WindowEvent::Focused(focused) => {
                let _ = self
                    .events
                    .send(DisplayEvent::Focused { window_id, focused });
                let _ = self.events.send(DisplayEvent::Modifiers {
                    modifiers: 0,
                    locks: locks(),
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some(scancode) = keycode_to_scancode(code) {
                        let _flags = key_flags(event.state, scancode.extended);
                        let _ = self.events.send(DisplayEvent::Key {
                            scancode: scancode.code,
                            pressed: event.state == ElementState::Pressed,
                            extended: scancode.extended,
                        });
                    }
                }
            }
            WindowEvent::ModifiersChanged(state) => {
                let _ = self.events.send(DisplayEvent::Modifiers {
                    modifiers: modifiers(state.state()),
                    locks: locks(),
                });
            }
            WindowEvent::CursorMoved { position, .. } => {
                let x = clamp_f64_to_i32(position.x);
                let y = clamp_f64_to_i32(position.y);
                self.pointer_position.insert(window_id, (x, y));
                let _ = self.events.send(DisplayEvent::Pointer {
                    window_id,
                    x,
                    y,
                    buttons: self.pointer_buttons,
                    wheel_x: 0,
                    wheel_y: 0,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.pointer_buttons = update_buttons(self.pointer_buttons, button, state);
                let (x, y) = self
                    .pointer_position
                    .get(&window_id)
                    .copied()
                    .unwrap_or((0, 0));
                let _ = self.events.send(DisplayEvent::Pointer {
                    window_id,
                    x,
                    y,
                    buttons: self.pointer_buttons,
                    wheel_x: 0,
                    wheel_y: 0,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (wheel_x, wheel_y) = wheel_delta(delta);
                let (x, y) = self
                    .pointer_position
                    .get(&window_id)
                    .copied()
                    .unwrap_or((0, 0));
                let _ = self.events.send(DisplayEvent::Pointer {
                    window_id,
                    x,
                    y,
                    buttons: self.pointer_buttons,
                    wheel_x,
                    wheel_y,
                });
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                let _ = self.events.send(DisplayEvent::Text { text });
            }
            WindowEvent::RedrawRequested => {
                let _ = self.presenter.refresh(window_id);
            }
            _ => {}
        }
    }
}

struct WinitBackendAdapter<'a> {
    inner: &'a mut WinitBackend,
    event_loop: &'a ActiveEventLoop,
}

impl DisplayBackend for WinitBackendAdapter<'_> {
    fn create_window(&mut self, remote: &RemoteWindow) -> Result<(), DisplayError> {
        if self.inner.windows.contains_key(&remote.window_id) {
            return Ok(());
        }

        let app_id = remote.app_id.clone();
        let mut attrs = Window::default_attributes()
            .with_title(remote.title.clone())
            .with_decorations(false)
            .with_inner_size(PhysicalSize::new(
                u32::from(remote.width),
                u32::from(remote.height),
            ))
            .with_resizable(remote.window_id != 0)
            .with_maximized(remote.maximized)
            .with_window_level(if remote_flag_topmost(remote) {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            });

        attrs = WindowAttributesExtX11::with_name(attrs, app_id.clone(), app_id.clone());
        attrs = WindowAttributesExtWayland::with_name(attrs, app_id.clone(), app_id);
        if self.event_loop.is_x11() {
            attrs = attrs.with_position(PhysicalPosition::new(remote.x, remote.y));
        }

        let window = Arc::new(
            self.event_loop
                .create_window(attrs)
                .map_err(display_error)?,
        );
        window.set_cursor_visible(self.inner.cursor_visible);
        let size = window.inner_size();
        self.inner
            .presenter
            .attach(remote.window_id, window.as_ref(), size.width, size.height)
            .map_err(display_error)?;

        let winit_id = window.id();
        let xid = x11_window_id(window.as_ref());
        self.inner.by_winit_id.insert(winit_id, remote.window_id);
        self.inner.windows.insert(
            remote.window_id,
            WinitWindow {
                window,
                xid,
                owner_id: remote.owner_id,
            },
        );

        self.inner.apply_transient_for(remote.window_id);
        Ok(())
    }

    fn destroy_window(&mut self, window_id: u32) -> Result<(), DisplayError> {
        self.inner.presenter.detach(window_id);
        if let Some(entry) = self.inner.windows.remove(&window_id) {
            self.inner.by_winit_id.remove(&entry.window.id());
        }
        Ok(())
    }

    fn move_window(&mut self, remote: &RemoteWindow) -> Result<(), DisplayError> {
        let Some(entry) = self.inner.windows.get(&remote.window_id) else {
            return Ok(());
        };
        let _ = entry.window.request_inner_size(PhysicalSize::new(
            u32::from(remote.width),
            u32::from(remote.height),
        ));
        self.inner
            .presenter
            .resize(
                remote.window_id,
                u32::from(remote.width),
                u32::from(remote.height),
            )
            .map_err(display_error)?;
        if self.event_loop.is_x11() {
            entry
                .window
                .set_outer_position(PhysicalPosition::new(remote.x, remote.y));
        }
        Ok(())
    }

    fn retitle_window(&mut self, remote: &RemoteWindow) -> Result<(), DisplayError> {
        if let Some(entry) = self.inner.windows.get(&remote.window_id) {
            entry.window.set_title(&remote.title);
        }
        Ok(())
    }

    fn change_state(&mut self, remote: &RemoteWindow) -> Result<(), DisplayError> {
        if let Some(entry) = self.inner.windows.get(&remote.window_id) {
            entry.window.set_minimized(remote.minimized);
            entry.window.set_maximized(remote.maximized);
            entry
                .window
                .set_window_level(if remote_flag_topmost(remote) {
                    WindowLevel::AlwaysOnTop
                } else {
                    WindowLevel::Normal
                });
        }
        Ok(())
    }

    fn change_icon(&mut self, remote: &RemoteWindow) -> Result<(), DisplayError> {
        if !self.event_loop.is_x11() {
            return Ok(());
        }
        let Some(icon) = remote.icon.as_ref() else {
            return Ok(());
        };
        let Some(entry) = self.inner.windows.get(&remote.window_id) else {
            return Ok(());
        };
        if let Some(rgba) = argb_to_rgba(&icon.argb, icon.width, icon.height) {
            if let Ok(icon) = Icon::from_rgba(rgba, u32::from(icon.width), u32::from(icon.height)) {
                entry.window.set_window_icon(Some(icon));
            }
        }
        Ok(())
    }

    fn restack(&mut self, _stack: &[u32]) -> Result<(), DisplayError> {
        Ok(())
    }

    fn frame(&mut self, frame: &FrameData) -> Result<Option<PresentTimes>, DisplayError> {
        if !self.inner.windows.contains_key(&frame.window_id) {
            return Ok(None);
        }
        match self.inner.presenter.present(frame.window_id, frame) {
            Ok(times) => {
                let _ = self.inner.events.send(DisplayEvent::Presented {
                    window_id: frame.window_id,
                    frame_id: frame.frame_id,
                    decoded_us: times.decoded_us,
                    presented_us: times.presented_us,
                });
                Ok(Some(times))
            }
            Err(PresentError::DroppedFrame { .. }) => Ok(None),
            Err(error) => Err(display_error(error)),
        }
    }

    fn cursor_shape(&mut self, shape: &CursorShape) -> Result<(), DisplayError> {
        if let Some(cursor) = self.inner.cursor_cache.get(&shape.cursor_id) {
            for entry in self.inner.windows.values() {
                entry.window.set_cursor(cursor.clone());
            }
            return Ok(());
        }
        if shape.format != cursor_format::BGRA_PREMUL {
            return Ok(());
        }
        let Some(rgba) = bgra_premul_to_rgba(&shape.data, shape.width, shape.height) else {
            return Ok(());
        };
        let Ok(source) = CustomCursor::from_rgba(
            rgba,
            shape.width,
            shape.height,
            shape.hotspot_x,
            shape.hotspot_y,
        ) else {
            return Ok(());
        };
        let cursor = self.event_loop.create_custom_cursor(source);
        for entry in self.inner.windows.values() {
            entry.window.set_cursor(cursor.clone());
        }
        self.inner.cursor_cache.insert(shape.cursor_id, cursor);
        Ok(())
    }

    fn cursor_moved(&mut self, _window_id: u32, _x: i32, _y: i32) -> Result<(), DisplayError> {
        Ok(())
    }

    fn cursor_visibility(&mut self, visible: bool) -> Result<(), DisplayError> {
        self.inner.cursor_visible = visible;
        for entry in self.inner.windows.values() {
            entry.window.set_cursor_visible(visible);
        }
        Ok(())
    }

    fn agent_error(&mut self, code: u16, message: &str) -> Result<(), DisplayError> {
        let _ = self.inner.events.send(DisplayEvent::BackendError {
            message: format!("agent error {code}: {message}"),
        });
        Ok(())
    }

    fn closed(&mut self) -> Result<(), DisplayError> {
        self.event_loop.exit();
        Ok(())
    }
}

impl WinitBackend {
    fn apply_transient_for(&mut self, window_id: u32) {
        let Some(entry) = self.windows.get(&window_id) else {
            return;
        };
        if entry.owner_id == 0 {
            return;
        }
        let (Some(child), Some(owner)) = (
            entry.xid,
            self.windows.get(&entry.owner_id).and_then(|w| w.xid),
        ) else {
            return;
        };
        let _ = self.x11_sidecar.set_transient_for(child, owner);
    }
}

struct WinitWindow {
    window: Arc<Window>,
    xid: Option<u32>,
    owner_id: u32,
}

struct X11Sidecar {
    conn: Option<x11rb::rust_connection::RustConnection>,
}

impl X11Sidecar {
    fn new() -> Self {
        Self { conn: None }
    }

    fn set_transient_for(&mut self, child: u32, owner: u32) -> Result<(), DisplayError> {
        if self.conn.is_none() {
            let (conn, _) = x11rb::connect(None).map_err(display_error)?;
            self.conn = Some(conn);
        }
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| DisplayError::new("missing X11 sidecar connection"))?;
        conn.change_property32(
            PropMode::REPLACE,
            child,
            AtomEnum::WM_TRANSIENT_FOR,
            AtomEnum::WINDOW,
            &[owner],
        )
        .map_err(display_error)?
        .check()
        .map_err(display_error)?;
        conn.flush().map_err(display_error)?;
        Ok(())
    }
}

fn remote_flag_topmost(remote: &RemoteWindow) -> bool {
    let _ = remote;
    false
}

fn x11_window_id(window: &Window) -> Option<u32> {
    let handle = window.window_handle().ok()?.as_raw();
    match handle {
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        _ => None,
    }
}

fn argb_to_rgba(data: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
    let expected = usize::from(width)
        .checked_mul(usize::from(height))?
        .checked_mul(4)?;
    if data.len() != expected {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        out.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    Some(out)
}

fn bgra_premul_to_rgba(data: &[u8], width: u16, height: u16) -> Option<Vec<u8>> {
    let expected = usize::from(width)
        .checked_mul(usize::from(height))?
        .checked_mul(4)?;
    if data.len() != expected {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        let b = unpremul(px[0], px[3]);
        let g = unpremul(px[1], px[3]);
        let r = unpremul(px[2], px[3]);
        out.extend_from_slice(&[r, g, b, px[3]]);
    }
    Some(out)
}

fn unpremul(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        0
    } else {
        let value = (u16::from(channel) * 255) / u16::from(alpha);
        u8::try_from(value.min(255)).expect("value is clamped to u8")
    }
}

fn wheel_delta(delta: MouseScrollDelta) -> (i16, i16) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (wheel_units(x), wheel_units(y)),
        MouseScrollDelta::PixelDelta(pos) => (
            wheel_units(pos.x as f32 / 120.0),
            wheel_units(pos.y as f32 / 120.0),
        ),
    }
}

fn wheel_units(lines: f32) -> i16 {
    let value = (lines * 120.0).round();
    if value > f32::from(i16::MAX) {
        i16::MAX
    } else if value < f32::from(i16::MIN) {
        i16::MIN
    } else {
        value as i16
    }
}

fn clamp_f64_to_i32(value: f64) -> i32 {
    if value > f64::from(i32::MAX) {
        i32::MAX
    } else if value < f64::from(i32::MIN) {
        i32::MIN
    } else {
        value.round() as i32
    }
}
