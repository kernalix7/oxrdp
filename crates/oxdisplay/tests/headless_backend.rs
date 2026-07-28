use oxdisplay::apply_display_command;
use oxdisplay::headless::{HeadlessBackend, HeadlessCall};
use oxproto::message::codec;
use oxproto::message::input::cursor_format;
use oxproto::message::window::frame_flag;
use oxproto::message::{CursorShape, FrameData, WindowIcon};

use oxdisplay::{DisplayCommand, WindowSpec};

fn apply(backend: &mut HeadlessBackend, command: DisplayCommand) -> Vec<HeadlessCall> {
    apply_display_command(backend, command).unwrap();
    backend.take_calls()
}

fn window(window_id: u32, owner_id: u32) -> WindowSpec {
    WindowSpec {
        window_id,
        app_id: "notepad.exe".to_owned(),
        title: format!("window {window_id}"),
        x: 10,
        y: 20,
        width: 640,
        height: 480,
        owner_id,
        minimized: false,
        maximized: false,
        resizable: true,
        has_frame: true,
        topmost: false,
        icon: None,
    }
}

#[test]
fn model_changes_drive_headless_backend_in_order() {
    let mut backend = HeadlessBackend::new();

    assert_eq!(
        apply(&mut backend, DisplayCommand::CreateWindow(window(1, 0))),
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
            &mut backend,
            DisplayCommand::RetitleWindow(WindowSpec {
                title: "renamed".to_owned(),
                ..window(1, 0)
            }),
        ),
        vec![HeadlessCall::Retitled {
            window_id: 1,
            title: "renamed".to_owned(),
        }]
    );

    assert_eq!(
        apply(
            &mut backend,
            DisplayCommand::MoveWindow(WindowSpec {
                x: 30,
                y: 40,
                width: 800,
                height: 600,
                ..window(1, 0)
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
            &mut backend,
            DisplayCommand::ChangeState(WindowSpec {
                maximized: true,
                ..window(1, 0)
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
            &mut backend,
            DisplayCommand::ChangeIcon(WindowSpec {
                icon: Some(WindowIcon {
                    window_id: 1,
                    width: 1,
                    height: 1,
                    argb: vec![255, 1, 2, 3],
                }),
                ..window(1, 0)
            }),
        ),
        vec![HeadlessCall::IconChanged(1)]
    );

    assert_eq!(
        apply(&mut backend, DisplayCommand::CreateWindow(window(2, 1))),
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
        apply(&mut backend, DisplayCommand::Restack(vec![2, 1])),
        vec![HeadlessCall::Restacked(vec![2, 1])]
    );

    assert_eq!(
        apply(&mut backend, DisplayCommand::DestroyWindow(1)),
        vec![HeadlessCall::Destroyed(1)]
    );
}

#[test]
fn frames_cursor_and_session_events_are_forwarded() {
    let mut backend = HeadlessBackend::new();
    let _ = apply(&mut backend, DisplayCommand::CreateWindow(window(9, 0)));

    assert_eq!(
        apply(
            &mut backend,
            DisplayCommand::Frame(FrameData {
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
            &mut backend,
            DisplayCommand::CursorShape(CursorShape {
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
            &mut backend,
            DisplayCommand::CursorPosition {
                window_id: 9,
                x: 4,
                y: 6,
            },
        ),
        vec![HeadlessCall::CursorMoved {
            window_id: 9,
            x: 4,
            y: 6,
        }]
    );

    assert_eq!(
        apply(&mut backend, DisplayCommand::CursorVisibility(false)),
        vec![HeadlessCall::CursorVisibility(false)]
    );

    assert_eq!(
        apply(
            &mut backend,
            DisplayCommand::AgentError {
                code: 12,
                message: "capture failed".to_owned(),
            },
        ),
        vec![HeadlessCall::AgentError {
            code: 12,
            message: "capture failed".to_owned(),
        }]
    );
}

#[test]
fn headless_backend_drops_frames_for_unknown_windows() {
    let mut backend = HeadlessBackend::new();

    assert_eq!(
        apply(
            &mut backend,
            DisplayCommand::Frame(FrameData {
                window_id: 99,
                frame_id: 1,
                codec: codec::RAW_BGRA,
                flags: frame_flag::KEYFRAME,
                width: 1,
                height: 1,
                captured_us: 0,
                encoded_us: 0,
                data: vec![0, 0, 0, 0],
            }),
        ),
        Vec::<HeadlessCall>::new()
    );
}
