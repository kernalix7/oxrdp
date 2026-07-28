use oxclient::{ClientEvent, WindowModel};
use oxdisplay::apply_model_change;
use oxdisplay::headless::{HeadlessBackend, HeadlessCall};
use oxproto::message::input::cursor_format;
use oxproto::message::window::{frame_flag, window_flag, window_show};
use oxproto::message::{codec, Error as AgentError};
use oxproto::message::{
    CursorPosition, CursorShape, CursorVisibility, FrameData, WindowClosed, WindowGeometry,
    WindowIcon, WindowOpened, WindowState, WindowTitle, WindowZOrder,
};

fn apply(
    model: &mut WindowModel,
    backend: &mut HeadlessBackend,
    event: ClientEvent,
) -> Vec<HeadlessCall> {
    for change in model.apply(event) {
        apply_model_change(backend, model, change).unwrap();
    }
    backend.take_calls()
}

fn opened(window_id: u32, owner_id: u32) -> WindowOpened {
    WindowOpened {
        window_id,
        video_channel: 16,
        pid: 7,
        app_id: "notepad.exe".to_owned(),
        title: format!("window {window_id}"),
        x: 10,
        y: 20,
        width: 640,
        height: 480,
        dpi: 96,
        flags: window_flag::RESIZABLE | window_flag::HAS_FRAME,
        owner_id,
    }
}

#[test]
fn model_changes_drive_headless_backend_in_order() {
    let mut model = WindowModel::new();
    let mut backend = HeadlessBackend::new();

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowOpened(opened(1, 0)),
        ),
        vec![HeadlessCall::Created {
            window_id: 1,
            app_id: "notepad.exe".to_owned(),
            title: "window 1".to_owned(),
            x: 10,
            y: 20,
            width: 640,
            height: 480,
            owner_id: 0,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowTitle(WindowTitle {
                window_id: 1,
                title: "renamed".to_owned(),
            }),
        ),
        vec![HeadlessCall::Retitled {
            window_id: 1,
            title: "renamed".to_owned(),
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowGeometry(WindowGeometry {
                window_id: 1,
                x: 30,
                y: 40,
                width: 800,
                height: 600,
            }),
        ),
        vec![HeadlessCall::Moved {
            window_id: 1,
            x: 30,
            y: 40,
            width: 800,
            height: 600,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowState(WindowState {
                window_id: 1,
                state: window_show::MAXIMIZED,
                flags: 0,
            }),
        ),
        vec![HeadlessCall::StateChanged {
            window_id: 1,
            minimized: false,
            maximized: true,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowIcon(WindowIcon {
                window_id: 1,
                width: 1,
                height: 1,
                argb: vec![255, 1, 2, 3],
            }),
        ),
        vec![HeadlessCall::IconChanged(1)]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowOpened(opened(2, 1)),
        ),
        vec![HeadlessCall::Created {
            window_id: 2,
            app_id: "notepad.exe".to_owned(),
            title: "window 2".to_owned(),
            x: 10,
            y: 20,
            width: 640,
            height: 480,
            owner_id: 1,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowZOrder(WindowZOrder {
                window_id: 1,
                above_window_id: 2,
            }),
        ),
        vec![HeadlessCall::Restacked(vec![2, 1])]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::WindowClosed(WindowClosed { window_id: 1 }),
        ),
        vec![HeadlessCall::Destroyed(1)]
    );
}

#[test]
fn frames_cursor_and_session_events_are_forwarded() {
    let mut model = WindowModel::new();
    let mut backend = HeadlessBackend::new();

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::Frame(FrameData {
                window_id: 9,
                frame_id: 42,
                codec: codec::RAW_BGRA,
                flags: frame_flag::KEYFRAME,
                width: 2,
                height: 1,
                captured_us: 10,
                encoded_us: 11,
                data: vec![0, 0, 255, 255, 0, 255, 0, 255],
            }),
        ),
        vec![HeadlessCall::Frame {
            window_id: 9,
            frame_id: 42,
            bytes: 8,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::CursorShape(CursorShape {
                cursor_id: 5,
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                format: cursor_format::BGRA_PREMUL,
                data: vec![0, 0, 0, 0],
            }),
        ),
        vec![HeadlessCall::CursorShape {
            cursor_id: 5,
            width: 1,
            height: 1,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::CursorPosition(CursorPosition {
                window_id: 9,
                x: 4,
                y: 6,
            }),
        ),
        vec![HeadlessCall::CursorMoved {
            window_id: 9,
            x: 4,
            y: 6,
        }]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::CursorVisibility(CursorVisibility { visible: false }),
        ),
        vec![HeadlessCall::CursorVisibility(false)]
    );

    assert_eq!(
        apply(
            &mut model,
            &mut backend,
            ClientEvent::Error(AgentError {
                code: 12,
                message: "capture failed".to_owned(),
            }),
        ),
        vec![HeadlessCall::AgentError {
            code: 12,
            message: "capture failed".to_owned(),
        }]
    );
}
