//! End-to-end through the shared client/core seam: the TUI opens and switches
//! between simulated serial devices (PTYs), writes without echo, sees AI
//! writes, and handles unplug and shutdown.

mod support;

use std::time::{Duration, Instant};

use serial_nexus_client::{Client, ClientInfo};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::Key;
use serial_nexus_terminal::state::{Focus, Overlay};
use support::{connect, start_core, SimulatedDevice};

fn app(socket: &std::path::PathBuf) -> App {
    let client = Client::connect_to_socket(socket, ClientInfo::human("serial-nexus-terminal-test"))
        .expect("terminal connects");
    App::new(client, 40, 120)
}

fn pump_until(app: &mut App, timeout: Duration, mut done: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        app.pump();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    app.pump();
    panic!("condition not met within {timeout:?}");
}

fn stream_text(app: &App) -> String {
    app.state
        .selected_session()
        .map(|session| session.screen.lines().join("\n"))
        .unwrap_or_default()
}

#[test]
fn opens_two_devices_switches_and_keeps_both_connected() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut first = SimulatedDevice::new();
    let mut second = SimulatedDevice::new();
    let mut app = app(&socket);

    // A closed device chosen in the sidebar asks for a baud rate.
    app.state.set_devices(vec![
        serial_nexus_terminal::state::DeviceEntry::new(&first.path),
        serial_nexus_terminal::state::DeviceEntry::new(&second.path),
    ]);
    app.handle_key(Key::Char('b'));
    assert!(app.state.prompt.is_some());
    for ch in "19200".chars() {
        app.handle_key(Key::Char(ch));
    }
    app.handle_key(Key::Enter);
    let first_id = app
        .state
        .selected_session()
        .expect("first device opened")
        .device_id;
    assert_eq!(app.state.selected_session().unwrap().baud, 19_200);
    assert_eq!(app.state.focus, Focus::Stream);

    app.handle_key(Key::Tab); // back to the sidebar
                              // Re-seed: PTYs are not enumerated, and the refresh after the first open
                              // drops the seeded closed entry for the second one.
    app.state.set_devices(vec![
        serial_nexus_terminal::state::DeviceEntry::new(&first.path),
        serial_nexus_terminal::state::DeviceEntry::new(&second.path),
    ]);
    app.state.select_path(&second.path);
    app.handle_key(Key::Enter); // closed: prompt again
    app.handle_key(Key::Enter); // empty input: documented default 115200
    let second_id = app
        .state
        .selected_session()
        .expect("second device opened")
        .device_id;
    assert_eq!(app.state.sessions.len(), 2);
    assert_eq!(app.state.selected_session().unwrap().baud, 115_200);

    // Typing goes to the selected device.
    app.handle_key(Key::Char('b'));
    second.read_until(b"b", Duration::from_secs(3));

    // Switch back: the first device is still attached (same device id), and
    // typing goes there without reopening.
    app.handle_key(Key::Tab); // sidebar
    app.state.select_path(&first.path);
    assert_eq!(app.state.selected_session().unwrap().device_id, first_id);
    app.handle_key(Key::Tab); // stream
    app.handle_key(Key::Char('a'));
    first.read_until(b"a", Duration::from_secs(3));
    assert_eq!(app.state.selected_session().unwrap().device_id, first_id);

    // Both are open on the core at their own rates.
    let devices = app.client().list_devices().expect("list").devices;
    let first_summary = devices
        .iter()
        .find(|device| device.path == first.path)
        .expect("first listed");
    assert!(first_summary.open);
    assert_eq!(first_summary.baud, Some(19_200));
    let second_summary = devices
        .iter()
        .find(|device| device.path == second.path)
        .expect("second listed");
    assert!(second_summary.open);
    assert_eq!(second_summary.baud, Some(115_200));
    assert!(app.client().status(first_id).is_ok());
    assert!(app.client().status(second_id).is_ok());

    app.shutdown();
}

#[test]
fn baud_conflict_is_reported_and_does_not_create_a_session() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    ai.open(&device.path, Some(57_600), true)
        .expect("AI opens at 57600");
    let mut app = app(&socket);
    app.refresh_devices();

    let error = app
        .open_path(&device.path, Some(9_600))
        .expect_err("wrong baud is refused");
    assert_eq!(
        error.code(),
        Some(serial_nexus_protocol::ErrorCode::BaudConflict)
    );
    assert!(app.state.sessions.is_empty());

    // Joining at the active rate is the supported path.
    app.state.select_path(&device.path);
    app.open_selected();
    assert_eq!(app.state.selected_session().unwrap().baud, 57_600);
    app.shutdown();
}

#[test]
fn joins_an_already_open_device_at_its_active_rate() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    let opened = ai
        .open(&device.path, Some(57_600), true)
        .expect("AI opens at 57600");
    let mut app = app(&socket);
    app.refresh_devices();
    assert!(app.state.select_path(&device.path), "device listed as open");

    app.open_selected();
    let session = app.state.selected_session().expect("joined");
    assert_eq!(session.device_id, opened.device_id);
    assert_eq!(session.baud, 57_600, "joined at the active rate");
    assert_eq!(session.control.as_deref(), Some("claude-desktop"));
    assert_eq!(session.screen.lines().len(), 40);

    // A human may write while an AI holds control, and does not take it.
    app.state.focus = Focus::Stream;
    app.handle_key(Key::Char('q'));
    device.read_until(b"q", Duration::from_secs(3));
    let status = ai.status(opened.device_id).expect("status");
    assert_eq!(
        status
            .control
            .holder
            .as_ref()
            .map(|holder| holder.name.as_deref()),
        Some(Some("claude-desktop"))
    );

    app.shutdown();
}

#[test]
fn human_typing_is_not_echoed_but_device_echo_and_ai_writes_are_visible() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    let mut app = app(&socket);
    app.open_path(&device.path, Some(115_200)).expect("open");
    app.state.focus = Focus::Stream;

    for ch in "hello".chars() {
        app.handle_key(Key::Char(ch));
    }
    device.read_until(b"hello", Duration::from_secs(3));
    app.pump();
    assert!(
        !stream_text(&app).contains("hello"),
        "human typing must not be echoed locally: {:?}",
        stream_text(&app)
    );

    // The device's own echo is device output and stays visible.
    device.write(b"hello");
    pump_until(&mut app, Duration::from_secs(3), |app| {
        stream_text(app).contains("hello")
    });
    assert!(!stream_text(&app).contains("[AI]"));

    // An AI write is shown inline, in its own style, with no source marker.
    let ai_open = ai.open(&device.path, None, true).expect("AI joins");
    assert_eq!(ai_open.baud, 115_200);
    ai.write_bytes(ai_open.device_id, b"AT+GMR\r\n", None)
        .expect("AI write");
    pump_until(&mut app, Duration::from_secs(3), |app| {
        stream_text(app).contains("AT+GMR")
    });

    app.shutdown();
}

#[test]
fn joining_does_not_duplicate_output_already_in_history() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    let opened = ai
        .open(&device.path, Some(115_200), true)
        .expect("AI opens");
    // Subscribe first, then let output arrive and reach history before the
    // open/join. The queued live event is delivered once; no snapshot replay.
    let mut app = app(&socket);
    device.write(b"ready\r\n");
    support::read_until_contains(&ai, opened.device_id, "ready", Duration::from_secs(3));
    app.open_path(&device.path, None).expect("join");
    pump_until(&mut app, Duration::from_secs(2), |app| {
        !stream_text(app).trim().is_empty()
    });
    let text = stream_text(&app);
    assert_eq!(text.matches("ready").count(), 1, "{text:?}");
    app.shutdown();
}

#[test]
fn unplug_is_reported_outside_the_stream_and_session_is_dropped() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let path = device.path.clone();
    let mut app = app(&socket);
    app.open_path(&path, Some(115_200)).expect("open");
    assert_eq!(app.state.sessions.len(), 1);

    drop(device);
    pump_until(&mut app, Duration::from_secs(5), |app| {
        app.state.sessions.is_empty()
    });
    let notice = app.state.notice.as_ref().expect("unplug notice");
    assert!(notice.text.contains("unplugged"), "{}", notice.text);
    assert!(notice.text.contains(&path), "{}", notice.text);

    // A write after the unplug is refused with a notice, not a local echo.
    app.handle_key(Key::Char('x'));
    app.pump();
    assert!(app.state.sessions.is_empty());
    assert!(app
        .state
        .notice
        .as_ref()
        .unwrap()
        .text
        .contains("no open device"));

    app.shutdown();
}

#[test]
fn named_ai_disconnect_clears_the_reported_control_state() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    let opened = ai
        .open(&device.path, Some(115_200), true)
        .expect("AI opens with control");
    let mut app = app(&socket);
    app.open_path(&device.path, None).expect("join");
    assert_eq!(
        app.state.selected_session().unwrap().control.as_deref(),
        Some("claude-desktop")
    );

    // The release event may not carry the holder's original name; the stable
    // client id must still clear it.
    ai.close(opened.device_id).expect("AI closes");
    pump_until(&mut app, Duration::from_secs(3), |app| {
        app.state
            .selected_session()
            .is_some_and(|session| session.control.is_none())
    });
    app.refresh_devices();
    let session = app.state.selected_session().expect("still attached");
    assert_eq!(session.control, None);
    let device = app.state.selected_device().expect("device row");
    assert_eq!(device.control, None);
    app.shutdown();
}

#[test]
fn rejoin_after_a_shared_device_disconnect_still_reports_the_next_unplug() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let path = device.path.clone();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    let opened = ai
        .open(&path, Some(115_200), true)
        .expect("AI keeps the device open");
    let mut app = app(&socket);
    app.open_path(&path, None).expect("join");
    assert_eq!(
        app.state.selected_session().unwrap().device_id,
        opened.device_id
    );

    // Disconnect while the AI keeps the device open: no DeviceClosed event.
    app.handle_key(Key::Tab);
    app.handle_key(Key::Char('d'));
    app.pump();
    assert!(app.state.sessions.is_empty());
    assert!(app.state.select_path(&path));

    // Rejoin the same core device id, then unplug for real.
    app.handle_key(Key::Enter);
    app.pump();
    assert_eq!(
        app.state.selected_session().expect("rejoined").device_id,
        opened.device_id
    );
    drop(device);
    pump_until(&mut app, Duration::from_secs(5), |app| {
        app.state.sessions.is_empty()
    });
    let notice = app.state.notice.as_ref().expect("unplug notice");
    assert!(notice.text.contains("unplugged"), "{}", notice.text);
    assert!(notice.text.contains(&path), "{}", notice.text);
    app.shutdown();
}

#[test]
fn shutdown_detaches_every_device() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let first = SimulatedDevice::new();
    let second = SimulatedDevice::new();
    let mut app = app(&socket);
    let first_id = app.open_path(&first.path, Some(115_200)).expect("open");
    let second_id = app.open_path(&second.path, Some(115_200)).expect("open");
    assert_eq!(app.state.sessions.len(), 2);

    app.shutdown();
    assert!(app.state.sessions.is_empty());
    for id in [first_id, second_id] {
        let error = app.client().status(id).expect_err("detached");
        assert!(error.is_device_gone(), "{error}");
    }
}

#[test]
fn explicit_disconnect_leaves_the_other_device_connected() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut first = SimulatedDevice::new();
    let second = SimulatedDevice::new();
    let mut app = app(&socket);
    let first_id = app.open_path(&first.path, Some(115_200)).expect("open");
    let second_id = app.open_path(&second.path, Some(115_200)).expect("open");

    app.handle_key(Key::Tab); // sidebar
    app.state.select_path(&first.path);
    app.handle_key(Key::Char('d'));
    app.pump();
    assert_eq!(app.state.sessions.len(), 1);
    let notice = app.state.notice.as_ref().expect("disconnect notice");
    assert!(
        notice.text.contains("disconnected"),
        "explicit disconnect must not be reported as unplug: {}",
        notice.text
    );
    assert!(!notice.text.contains("unplugged"), "{}", notice.text);
    assert!(app.state.sessions.contains_key(&second_id));
    assert!(app.client().status(first_id).is_err());
    assert!(app.client().status(second_id).is_ok());

    // The first device was released, so it can be opened again.
    app.state.select_path(&first.path);
    app.open_path(&first.path, Some(115_200)).expect("reopen");
    first.write(b"back");
    pump_until(&mut app, Duration::from_secs(3), |app| {
        stream_text(app).contains("back")
    });

    app.shutdown();
}

#[test]
fn launch_settings_are_loaded_into_the_app_and_do_not_change_baud_behavior() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let settings = serial_nexus_terminal::settings::TerminalSettings {
        local_echo: false,
        scrollback: false,
        ..Default::default()
    };
    let client = Client::connect_to_socket(&socket, ClientInfo::human("terminal-settings"))
        .expect("terminal connects");
    let mut app = App::with_settings(client, 40, 120, settings.clone());
    assert_eq!(app.settings(), &settings);

    // Settings do not touch device opening: an empty baud prompt still asks
    // the core for the documented default rate.
    app.state
        .set_devices(vec![serial_nexus_terminal::state::DeviceEntry::new(
            &device.path,
        )]);
    app.handle_key(Key::Char('b'));
    app.handle_key(Key::Enter);
    assert_eq!(app.state.selected_session().expect("opened").baud, 115_200);

    app.shutdown();
}

#[test]
fn local_echo_shows_typed_keys_only_while_it_is_on() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let settings = serial_nexus_terminal::settings::TerminalSettings {
        local_echo: true,
        scrollback: true,
        ..Default::default()
    };
    let client = Client::connect_to_socket(&socket, ClientInfo::human("terminal-echo"))
        .expect("terminal connects");
    let mut app = App::with_settings(client, 40, 120, settings);
    app.open_path(&device.path, Some(115_200)).expect("open");
    app.state.focus = Focus::Stream;

    for ch in "ab".chars() {
        app.handle_key(Key::Char(ch));
    }
    device.read_until(b"ab", Duration::from_secs(3));
    assert!(
        stream_text(&app).contains("ab"),
        "local echo is on: {:?}",
        stream_text(&app)
    );

    // Turning it off in the settings panel stops the local echo at once.
    app.handle_key(Key::F(2));
    assert_eq!(app.state.overlay, Some(Overlay::Settings));
    app.handle_key(Key::Char(' '));
    assert!(!app.state.settings.local_echo);
    app.handle_key(Key::Esc);
    for ch in "cd".chars() {
        app.handle_key(Key::Char(ch));
    }
    device.read_until(b"cd", Duration::from_secs(3));
    app.pump();
    assert!(
        !stream_text(&app).contains("cd"),
        "local echo is off: {:?}",
        stream_text(&app)
    );

    app.shutdown();
}

#[test]
fn a_setting_changed_in_the_panel_is_written_back_and_kept_for_the_next_launch() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terminal.json");
    let store = serial_nexus_terminal::settings::SettingsStore::at(&path);
    let settings = store.load_or_create().expect("settings file");

    let client = Client::connect_to_socket(&socket, ClientInfo::human("terminal-save"))
        .expect("terminal connects");
    let mut app = App::with_preferences(client, 40, 120, settings, Some(store.clone()));
    app.handle_key(Key::F(2));
    assert_eq!(app.state.overlay, Some(Overlay::Settings));
    // Local echo is the first row and starts off.
    app.handle_key(Key::Char(' '));
    assert!(app.state.settings.local_echo);
    assert!(
        store.load().expect("load").expect("file").local_echo,
        "the change was saved"
    );

    // A failing save is reported, and the preference still applies here.
    let blocked = dir.path().join("not-a-directory");
    std::fs::write(&blocked, "x").unwrap();
    let mut app = App::with_preferences(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-save-fail"))
            .expect("terminal connects"),
        40,
        120,
        store.load().unwrap().unwrap(),
        Some(serial_nexus_terminal::settings::SettingsStore::at(
            blocked.join("terminal.json"),
        )),
    );
    app.handle_key(Key::F(2));
    app.handle_key(Key::Char(' '));
    assert!(!app.state.settings.local_echo);
    let notice = app.state.notice.as_ref().expect("save failure reported");
    assert!(notice.text.contains("not saved"), "{}", notice.text);

    app.shutdown();
}
