//! End-to-end through the shared client/core seam: the TUI opens and switches
//! between simulated serial devices (PTYs), writes without echo, sees AI
//! writes, and handles unplug and shutdown.

mod support;

use std::time::{Duration, Instant};

use serial_nexus_client::{Client, ClientInfo};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::Key;
use serial_nexus_terminal::state::AppState;
use serial_nexus_terminal::state::View;
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
    assert_eq!(app.state.view, View::Stream);

    app.handle_key(Key::Ctrl(']')); // back to the sidebar
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
    app.handle_key(Key::Ctrl(']')); // sidebar
    app.state.select_path(&first.path);
    assert_eq!(app.state.selected_session().unwrap().device_id, first_id);
    app.handle_key(Key::Ctrl(']')); // stream
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
    app.state.view = View::Stream;
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
fn with_local_echo_off_typing_is_not_echoed_but_device_echo_and_ai_writes_are_visible() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let ai = connect(&socket, ClientInfo::ai("claude-desktop"));
    let settings = serial_nexus_terminal::settings::TerminalSettings {
        local_echo: false,
        scrollback: true,
    };
    let client = Client::connect_to_socket(&socket, ClientInfo::human("terminal-no-echo"))
        .expect("terminal connects");
    let mut app = App::with_settings(client, 40, 120, settings);
    app.open_path(&device.path, Some(115_200)).expect("open");

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
    app.handle_key(Key::Ctrl(']'));
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

    app.handle_key(Key::Ctrl(']')); // sidebar
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
fn local_echo_is_on_by_default_and_stops_when_it_is_turned_off() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let config = tempfile::tempdir().unwrap();
    let store =
        serial_nexus_terminal::settings::SettingsStore::at(config.path().join("terminal.json"));
    let settings = store.load_or_create().expect("settings file");
    let mut app = App::with_preferences(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-echo-default"))
            .expect("terminal connects"),
        40,
        120,
        settings,
        Some(store),
    );
    assert!(app.settings().local_echo, "local echo is on by default");
    app.open_path(&device.path, Some(115_200)).expect("open");
    assert_eq!(app.state.view, View::Stream);

    for ch in "ab".chars() {
        app.handle_key(Key::Char(ch));
    }
    device.read_until(b"ab", Duration::from_secs(3));
    assert!(
        stream_text(&app).contains("ab"),
        "local echo is on: {:?}",
        stream_text(&app)
    );

    // The sidebar button opens the settings; the change is a draft until
    // Ctrl+S keeps it.
    app.handle_key(Key::Ctrl(']'));
    assert_eq!(app.state.view, View::Devices);
    app.state.select_settings_button();
    app.handle_key(Key::Enter);
    assert_eq!(app.state.view, View::Settings);
    app.handle_key(Key::Char(' '));
    assert!(!app.state.draft.local_echo, "the draft flips");
    assert!(
        app.state.settings.local_echo,
        "nothing is in effect before saving"
    );
    app.handle_key(Key::Ctrl('s'));
    assert_eq!(app.state.view, View::Devices);
    assert!(!app.state.settings.local_echo, "Ctrl+S keeps the change");

    app.handle_key(Key::Ctrl(']'));
    assert_eq!(app.state.view, View::Stream);
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
fn settings_escape_throws_the_draft_away_and_shows_both_flags() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut app = app(&socket);
    app.open_settings();
    assert_eq!(app.state.view, View::Settings);
    // Both flags are there, scrollback included, before it scrolls anything.
    assert_eq!(AppState::settings_row_name(0), Some("Local echo"));
    assert_eq!(AppState::settings_row_name(1), Some("Scrollback"));
    app.handle_key(Key::Char(' '));
    assert!(!app.state.draft.local_echo);
    app.handle_key(Key::Tab);
    assert_eq!(app.state.settings_row, 1);
    app.handle_key(Key::Char(' '));
    assert!(!app.state.draft.scrollback);
    // Escape keeps neither change.
    assert_eq!(
        app.state.settings,
        serial_nexus_terminal::settings::TerminalSettings::default()
    );
    app.handle_key(Key::Esc);
    assert_eq!(app.state.view, View::Devices);
    assert_eq!(
        app.state.settings,
        serial_nexus_terminal::settings::TerminalSettings::default()
    );
    app.shutdown();
}

#[test]
fn the_stream_owns_tab_and_ctrl_q_and_the_sidebar_ctrl_q_quits() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let mut app = app(&socket);
    app.open_path(&device.path, Some(115_200)).expect("open");
    assert_eq!(app.state.view, View::Stream);

    // Live keys are the device's, Tab and Ctrl+Q included.
    app.handle_key(Key::Tab);
    device.read_until(b"\t", Duration::from_secs(3));
    app.handle_key(Key::Ctrl('q'));
    device.read_until(&[0x11], Duration::from_secs(3));
    app.pump();
    assert!(
        !app.state.quit,
        "Ctrl+Q in the stream is the device's XON/XOFF"
    );

    // Back in the sidebar, Ctrl+Q is the terminal's quit, and `]` sends the
    // literal Ctrl+] the stream cannot send.
    app.handle_key(Key::Ctrl(']'));
    assert_eq!(app.state.view, View::Devices);
    app.handle_key(Key::Char(']'));
    device.read_until(&[0x1d], Duration::from_secs(3));
    app.handle_key(Key::Ctrl('q'));
    assert!(app.state.quit, "Ctrl+Q quits from the sidebar");

    app.shutdown();
}

#[test]
fn a_setting_changed_in_the_settings_view_is_saved_on_ctrl_s() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terminal.json");
    let store = serial_nexus_terminal::settings::SettingsStore::at(&path);
    let settings = store.load_or_create().expect("settings file");

    let client = Client::connect_to_socket(&socket, ClientInfo::human("terminal-save"))
        .expect("terminal connects");
    let mut app = App::with_preferences(client, 40, 120, settings, Some(store.clone()));
    // The keyboard route: the Settings button is the last sidebar row.
    app.state.select_settings_button();
    app.handle_key(Key::Enter);
    assert_eq!(app.state.view, View::Settings);
    app.handle_key(Key::Char(' '));
    assert!(
        store.load().expect("load").expect("file").local_echo,
        "the draft alone is not written"
    );
    app.handle_key(Key::Ctrl('s'));
    assert!(
        !store.load().expect("load").expect("file").local_echo,
        "Ctrl+S wrote the change"
    );

    app.shutdown();
}

#[test]
fn a_failed_settings_save_neither_applies_nor_claims_the_change() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terminal.json");
    let store = serial_nexus_terminal::settings::SettingsStore::at(&path);
    store
        .save(&serial_nexus_terminal::settings::TerminalSettings {
            local_echo: true,
            scrollback: true,
        })
        .expect("a settings file to keep");
    let before = std::fs::read_to_string(&path).expect("the file");

    let mut app = App::with_preferences(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-save-fail"))
            .expect("terminal connects"),
        40,
        120,
        store.load().expect("load").expect("file"),
        Some(store.clone()),
    );

    // Make the save fail: the directory cannot take the staging file.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    app.state.select_settings_button();
    app.handle_key(Key::Enter);
    app.handle_key(Key::Char(' '));
    assert!(!app.state.draft.local_echo, "the draft was flipped");
    app.handle_key(Key::Ctrl('s'));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        app.state.settings.local_echo,
        "the active options are unchanged by a failed save"
    );
    assert_eq!(
        app.state.view,
        View::Settings,
        "the settings stay open with the draft, not reported as saved"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file is untouched"),
        before,
        "the persisted file is unchanged"
    );
    let notice = app.state.notice.as_ref().expect("save failure reported");
    assert!(
        notice.text.contains("not saved"),
        "the failure is reported: {}",
        notice.text
    );

    // Without a settings file there is nothing to save either, and no claim.
    let mut app = App::with_settings(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-no-store"))
            .expect("terminal connects"),
        40,
        120,
        store.load().expect("load").expect("file"),
    );
    app.open_settings();
    app.handle_key(Key::Char(' '));
    app.handle_key(Key::Ctrl('s'));
    assert!(app.state.settings.local_echo, "no store, no change applied");
    assert_eq!(app.state.view, View::Settings);
    let notice = app.state.notice.as_ref().expect("no store reported");
    assert!(
        notice.text.contains("not saved"),
        "the missing file is reported as a failed save: {}",
        notice.text
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file is untouched"),
        before
    );

    app.shutdown();
}

#[test]
fn clicking_the_settings_button_opens_the_settings_and_a_device_row_selects_it() {
    use ratatui::layout::Rect;
    use serial_nexus_terminal::render::{settings_button, sidebar};

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let mut app = app(&socket);
    app.open_path(&device.path, Some(115_200)).expect("open");
    app.handle_key(Key::Ctrl(']'));
    assert_eq!(app.state.view, View::Devices);

    let area = Rect::new(0, 0, 100, 16);
    let button = settings_button(area).expect("the button is drawn");
    app.handle_click(area, button.x + 2, button.y);
    assert_eq!(app.state.view, View::Settings);

    app.handle_key(Key::Esc);
    assert_eq!(app.state.view, View::Devices);
    let list = sidebar(area).expect("the list is drawn").list;
    app.handle_click(area, list.x, list.y);
    assert_eq!(app.state.selected, 0, "the first device row is selected");
    // A click outside the controls does nothing.
    app.handle_click(area, 0, 0);
    assert_eq!(app.state.selected, 0);

    app.shutdown();
}

#[test]
fn settings_clicked_during_a_baud_prompt_takes_the_keys_and_cancels_the_prompt() {
    use ratatui::layout::Rect;
    use serial_nexus_terminal::render::settings_button;

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let config = tempfile::tempdir().unwrap();
    let store =
        serial_nexus_terminal::settings::SettingsStore::at(config.path().join("terminal.json"));
    let settings = store.load_or_create().expect("settings file");
    let mut app = App::with_preferences(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-prompt-settings"))
            .expect("terminal connects"),
        40,
        120,
        settings,
        Some(store),
    );
    app.state
        .set_devices(vec![serial_nexus_terminal::state::DeviceEntry::new(
            &device.path,
        )]);
    app.handle_key(Key::Char('b'));
    assert!(app.state.prompt.is_some());

    let area = Rect::new(0, 0, 100, 16);
    let button = settings_button(area).expect("button");
    app.handle_click(area, button.x + 2, button.y);
    assert_eq!(app.state.view, View::Settings);
    assert!(app.state.prompt.is_none(), "the prompt is abandoned");

    // The settings keys reach the settings, not the prompt.
    app.handle_key(Key::Char(' '));
    assert!(!app.state.draft.local_echo);
    app.handle_key(Key::Ctrl('s'));
    assert_eq!(app.state.view, View::Devices);
    assert!(app.state.sessions.is_empty(), "no device was opened");
    app.shutdown();
}

#[test]
fn an_ordinary_ctrl_bracket_byte_switches_the_view() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serial_nexus_terminal::input::translate;

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let mut app = app(&socket);
    app.open_path(&device.path, Some(115_200)).expect("open");
    assert_eq!(app.state.view, View::Stream);

    // What a plain terminal delivers for the 0x1d byte, and what the enhanced
    // protocol delivers, both route through the same translation.
    for char in ['5', ']'] {
        let key =
            translate(KeyEvent::new(KeyCode::Char(char), KeyModifiers::CONTROL)).expect("a key");
        app.handle_key(key);
        assert_eq!(app.state.view, View::Devices, "Ctrl+{char} is Ctrl+]");
        app.handle_key(Key::Ctrl(']'));
        assert_eq!(app.state.view, View::Stream);
    }
    app.shutdown();
}

#[test]
fn opening_settings_does_not_move_typing_to_another_attached_device() {
    use ratatui::layout::Rect;
    use serial_nexus_terminal::render::sidebar;

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let first = SimulatedDevice::new();
    let second = SimulatedDevice::new();
    let mut app = app(&socket);
    app.open_path(&first.path, Some(115_200))
        .expect("open first");
    app.open_path(&second.path, Some(115_200))
        .expect("open second");
    // Both sessions are listed as open devices, sorted by path.
    app.state.set_devices(Vec::new());
    app.handle_key(Key::Ctrl(']'));
    let sidebar = sidebar(Rect::new(0, 0, 100, 16)).expect("sidebar");
    // Click the first row, then open Settings from the button.
    app.handle_click(Rect::new(0, 0, 100, 16), sidebar.list.x, sidebar.list.y);
    let selected = app.state.selected;
    app.state.select_settings_button();
    app.handle_key(Key::Enter);
    app.handle_key(Key::Esc);
    assert_eq!(app.state.selected, selected, "the device row is restored");

    // Ctrl+] with the Settings button selected still opens the attached
    // device's stream rather than complaining.
    app.state.select_settings_button();
    app.handle_key(Key::Ctrl(']'));
    assert_eq!(app.state.view, View::Stream);
    assert!(app.state.selected_session().is_some());

    app.shutdown();
}

#[test]
fn the_settings_button_keeps_its_focus_when_discovery_grows_the_list() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let first = SimulatedDevice::new();
    let mut app = app(&socket);
    app.open_path(&first.path, Some(115_200)).expect("open");
    app.state.set_devices(Vec::new());
    app.handle_key(Key::Ctrl(']'));
    app.state.select_settings_button();
    let device = app.state.selected_device().map(|device| device.device_id);
    assert_eq!(device, None, "the button has the focus");

    // Discovery finds a device that sorts before the attached one, as another
    // AI opening one would make it do. The attached session keeps its entry.
    let mut path = first.path.clone();
    path.push_str("-aaa");
    app.state
        .set_devices(vec![serial_nexus_terminal::state::DeviceEntry::new(&path)]);
    assert!(
        app.state.settings_button_selected(),
        "the button still has the focus after discovery"
    );

    // Enter still opens the settings, not a device.
    app.handle_key(Key::Enter);
    assert_eq!(app.state.view, View::Settings);
    app.handle_key(Key::Esc);

    // Ctrl+] without moving the focus shows the attached device's stream.
    app.state.select_settings_button();
    app.handle_key(Key::Ctrl(']'));
    assert_eq!(app.state.view, View::Stream);
    assert!(app.state.selected_device().is_some());

    app.shutdown();
}

#[test]
fn the_settings_page_has_mouse_toggles_and_a_mouse_save_and_cancel() {
    use ratatui::layout::Rect;
    use serial_nexus_terminal::render::settings_layout;

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let config = tempfile::tempdir().unwrap();
    let store =
        serial_nexus_terminal::settings::SettingsStore::at(config.path().join("terminal.json"));
    let settings = store.load_or_create().expect("settings file");
    let mut app = App::with_preferences(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-mouse-settings"))
            .expect("terminal connects"),
        40,
        120,
        settings,
        Some(store.clone()),
    );
    app.open_path(&device.path, Some(115_200)).expect("open");
    let area = Rect::new(0, 0, 100, 16);

    // The sidebar's Settings button, clicked with the mouse.
    app.handle_key(Key::Ctrl(']'));
    let button = serial_nexus_terminal::render::settings_button(area).expect("button");
    app.handle_click(area, button.x + 2, button.y);
    assert_eq!(app.state.view, View::Settings);

    let layout = settings_layout(area).expect("settings layout");
    assert!(layout.save.width > 0 && layout.cancel.width > 0);

    // A click on the first flag toggles the draft, not the active options.
    app.handle_click(area, layout.rows.x, layout.rows.y);
    assert!(!app.state.draft.local_echo, "the click flipped the flag");
    assert!(
        app.state.settings.local_echo,
        "the active option is unchanged"
    );
    assert!(
        store.load().expect("load").expect("file").local_echo,
        "nothing was written before Save"
    );

    // Cancel throws the draft away and writes nothing.
    app.handle_click(area, layout.cancel.x + 1, layout.cancel.y);
    assert_eq!(app.state.view, View::Devices);
    assert!(app.state.settings.local_echo);
    assert!(app.state.draft.local_echo);
    assert!(store.load().expect("load").expect("file").local_echo);

    // Save writes the file and only then applies the change.
    app.handle_click(area, button.x + 2, button.y);
    let layout = settings_layout(area).expect("settings layout");
    app.handle_click(area, layout.rows.x, layout.rows.y);
    assert!(!app.state.draft.local_echo);
    app.handle_click(area, layout.save.x + 1, layout.save.y);
    assert_eq!(app.state.view, View::Devices, "Save leaves the settings");
    assert!(!app.state.settings.local_echo, "Save applied the change");
    assert!(!store.load().expect("load").expect("file").local_echo);

    app.shutdown();
}

#[test]
fn a_mouse_save_that_fails_changes_nothing_and_stays_on_the_page() {
    use std::os::unix::fs::PermissionsExt;

    use ratatui::layout::Rect;
    use serial_nexus_terminal::render::settings_layout;

    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let config = tempfile::tempdir().unwrap();
    let path = config.path().join("terminal.json");
    let store = serial_nexus_terminal::settings::SettingsStore::at(&path);
    store
        .save(&serial_nexus_terminal::settings::TerminalSettings {
            local_echo: true,
            scrollback: true,
        })
        .expect("a settings file to keep");
    let before = std::fs::read_to_string(&path).expect("the file");

    let mut app = App::with_preferences(
        Client::connect_to_socket(&socket, ClientInfo::human("terminal-mouse-save-fail"))
            .expect("terminal connects"),
        40,
        120,
        store.load().expect("load").expect("file"),
        Some(store.clone()),
    );
    app.open_path(&device.path, Some(115_200)).expect("open");
    let area = Rect::new(0, 0, 100, 16);
    app.open_settings();
    let layout = settings_layout(area).expect("settings layout");
    app.handle_click(area, layout.rows.x, layout.rows.y);
    assert!(!app.state.draft.local_echo);

    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    app.handle_click(area, layout.save.x + 1, layout.save.y);
    std::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_eq!(
        app.state.view,
        View::Settings,
        "a failed save keeps the page and the draft"
    );
    assert!(
        app.state.settings.local_echo,
        "the active option is unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file is untouched"),
        before
    );
    let notice = app.state.notice.as_ref().expect("the failure is reported");
    assert!(notice.text.contains("not saved"), "{}", notice.text);

    app.shutdown();
}

/// The view as the stream draws it: the live screen, or the rows the view is
/// scrolled back to.
fn view_text(app: &App) -> String {
    let Some(session) = app.state.selected_session() else {
        return String::new();
    };
    let screen = &session.screen;
    (0..screen.rows())
        .map(|row| {
            (0..screen.cols())
                .map(|col| screen.view_cell(row, col).ch)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_stream_scrolls_back_through_device_output_and_returns_to_live() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let mut app = app(&socket);
    app.open_path(&device.path, Some(115_200)).expect("open");
    app.resize_stream(6, 40);

    let lines: String = (0..12).map(|index| format!("line{index}\r\n")).collect();
    device.write(lines.as_bytes());
    pump_until(&mut app, Duration::from_secs(3), |app| {
        stream_text(app).contains("line11")
    });
    assert!(
        app.state.selected_session().unwrap().screen.history_len() > 0,
        "the rows above the live view were kept"
    );

    // Shift+PageUp is the terminal's own key: a page back, not a device write.
    app.handle_key(Key::ShiftPageUp);
    let session = app.state.selected_session().unwrap();
    assert!(session.screen.scroll() > 0);
    let scrolled = view_text(&app);
    assert!(scrolled.contains("line1"), "{scrolled}");
    assert!(!scrolled.contains("line11"), "{scrolled}");

    // Shift+Home shows the oldest kept line; Shift+End jumps back to live.
    app.handle_key(Key::ShiftHome);
    assert!(view_text(&app).contains("line0"), "{}", view_text(&app));
    app.handle_key(Key::ShiftEnd);
    assert_eq!(app.state.selected_session().unwrap().screen.scroll(), 0);
    assert!(view_text(&app).contains("line11"));

    // Typing goes to the device and brings the view back to live.
    app.handle_key(Key::ShiftPageUp);
    assert!(app.state.selected_session().unwrap().screen.scroll() > 0);
    app.handle_key(Key::Char('x'));
    device.read_until(b"x", Duration::from_secs(3));
    assert_eq!(
        app.state.selected_session().unwrap().screen.scroll(),
        0,
        "typing shows the live view again"
    );

    app.shutdown();
}

#[test]
fn without_scrollback_output_is_not_kept_and_the_keys_do_nothing() {
    let (_dir, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let client = Client::connect_to_socket(&socket, ClientInfo::human("terminal-no-scrollback"))
        .expect("terminal connects");
    let settings = serial_nexus_terminal::settings::TerminalSettings {
        local_echo: true,
        scrollback: false,
    };
    let mut app = App::with_settings(client, 40, 120, settings);
    app.open_path(&device.path, Some(115_200)).expect("open");
    app.resize_stream(6, 40);

    let lines: String = (0..12).map(|index| format!("line{index}\r\n")).collect();
    device.write(lines.as_bytes());
    pump_until(&mut app, Duration::from_secs(3), |app| {
        stream_text(app).contains("line11")
    });
    assert_eq!(
        app.state.selected_session().unwrap().screen.history_len(),
        0,
        "the setting turns keeping output off"
    );
    app.handle_key(Key::ShiftPageUp);
    assert_eq!(app.state.selected_session().unwrap().screen.scroll(), 0);

    app.shutdown();
}
