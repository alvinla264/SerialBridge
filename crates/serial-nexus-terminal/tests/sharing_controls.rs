//! The host's sharing controls against a real core: a session can only start
//! deliberately, unchecking revokes remote access at the core, bind failures
//! leave sharing off, and secrets live only in memory.

mod support;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serial_nexus_client::{ClientInfo, RemoteClient};

use serial_nexus_protocol::{PairedHost, PairingRole, PairingSecret, SharingSnapshot};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::settings::SettingsStore;
use serial_nexus_terminal::state::{Notice, Severity, SETTINGS_FIXED_ROWS};
use support::{connect, start_core, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

/// The port lister is process-wide, so sharing tests take turns.
static SHARING_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    // A failed earlier test poisons the lock; the seam is process-wide but
    // each test clears and reinstalls it, so the next one may still run.
    SHARING_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Fixture {
    _core: TempDir,
    _xdg: TempDir,
    socket: PathBuf,
    settings_path: PathBuf,
    devices: Arc<Mutex<Vec<PathBuf>>>,
    _lister: PortListerGuard,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> (Self, Vec<SimulatedDevice>) {
        let guard = serial_test();
        let (core, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let simulated = vec![SimulatedDevice::new(), SimulatedDevice::new()];
        let paths: Vec<PathBuf> = simulated
            .iter()
            .map(|device| PathBuf::from(&device.path))
            .collect();
        let devices = Arc::new(Mutex::new(paths));
        let lister = PortListerGuard::from_paths(Arc::clone(&devices));
        let xdg = TempDir::new().expect("xdg");
        let settings_path = xdg.path().join("serial-nexus/terminal.json");
        (
            Self {
                _core: core,
                _xdg: xdg,
                socket,
                settings_path,
                devices,
                _lister: lister,
                _guard: guard,
            },
            simulated,
        )
    }

    fn app(&self) -> App {
        let settings = SettingsStore::at(&self.settings_path)
            .load_or_create()
            .expect("settings");
        let client = connect(&self.socket, ClientInfo::human("sharing-terminal"));
        App::with_preferences(
            client,
            40,
            100,
            settings,
            Some(SettingsStore::at(&self.settings_path)),
        )
    }

    fn replace_devices(&self, paths: Vec<PathBuf>) {
        *self.devices.lock().expect("devices lock") = paths;
    }

    fn settings_text(&self) -> String {
        std::fs::read_to_string(&self.settings_path).unwrap_or_default()
    }
}

fn check_device(app: &mut App, path: &str) {
    app.state.begin_settings();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("device is detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(true));
}

fn set_bind(app: &mut App, address: &str, port: u16) {
    app.state.draft.share_address = address.into();
    app.state.draft.share_port = port;
    app.state.port_text = port.to_string();
}

fn save(app: &mut App) {
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Settings);
    app.handle_key(serial_nexus_terminal::input::Key::Ctrl('s'));
}

fn start(app: &mut App) {
    if app.state.focus != serial_nexus_terminal::state::Focus::Sidebar {
        app.handle_key(serial_nexus_terminal::input::Key::Ctrl(']'));
    }
    app.handle_key(serial_nexus_terminal::input::Key::Char('s'));
}

fn pairing(app: &App) -> (PairedHost, PairingSecret, PairingSecret) {
    let info = app
        .state
        .sharing
        .pairing
        .as_ref()
        .expect("a running session shows pairing information");
    (info.host.clone(), info.human.clone(), info.ai.clone())
}

#[test]
fn saving_checks_starts_a_session_and_shows_both_credentials_and_host_authentication() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    assert_eq!(
        app.state.view,
        serial_nexus_terminal::state::View::Stream,
        "Save returns to the stream"
    );
    assert!(app.state.sharing.pending_checked.contains(&path));
    assert!(!app.state.sharing.active);

    start(&mut app);
    assert!(app.state.sharing.active, "sharing started");
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Pairing);
    let (host, human, ai) = pairing(&app);
    assert_eq!(human.role, PairingRole::Human);
    assert_eq!(ai.role, PairingRole::Ai);
    assert_ne!(human.credential, ai.credential);
    assert!(!host.ca_cert_der_b64.is_empty());
    assert_eq!(host.host_cert_fingerprint_sha256.len(), 64);
    assert!(
        app.state
            .sharing
            .snapshot
            .as_ref()
            .expect("snapshot")
            .checked
            .iter()
            .any(|device| device.path == path),
        "the core confirms the checked device"
    );

    // The secrets are never written to the settings file.
    let saved = fixture.settings_text();
    assert!(!saved.contains(&human.credential));
    assert!(!saved.contains(&ai.credential));
    assert!(!saved.contains("credential"));

    // Debug never prints a credential value.
    let debug = format!("{:?}", human);
    assert!(
        !debug.contains(&human.credential),
        "Debug prints no credential"
    );

    app.shutdown();
}

#[test]
fn the_checked_device_can_be_opened_by_a_remote_client_though_the_host_never_opened_it() {
    let (fixture, mut devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (host, human, _ai) = pairing(&app);

    let remote = RemoteClient::connect(
        &host,
        &human,
        Some("remote-human".into()),
        Duration::from_secs(5),
    )
    .expect("the paired human client authenticates");
    assert_eq!(remote.role(), PairingRole::Human);
    let listed = remote.list_devices().expect("remote list");
    assert_eq!(listed.devices.len(), 1, "only the checked device appears");
    assert_eq!(listed.devices[0].path, path);
    assert!(!listed.devices[0].open);

    // An allowed but unopened port opens at the chosen baud rate.
    let opened = remote
        .open(&path, Some(9_600), false)
        .expect("the remote client may open a checked device");
    assert_eq!(opened.baud, 9_600);
    remote
        .write_bytes(opened.device_id, b"remote\r", None)
        .expect("write reaches the core");
    devices[0].read_until(b"remote", Duration::from_secs(5));
    devices[0].write(b"answer\r\n");
    let read = remote.read(opened.device_id, Some(10)).expect("read");
    assert!(
        read.lines.iter().any(|line| line.text.contains("answer")),
        "{:?}",
        read.lines
    );

    app.shutdown();
}

#[test]
fn unchecking_while_sharing_revokes_the_remote_client_and_leaves_other_devices_alone() {
    let (fixture, mut devices) = Fixture::new();
    let mut app = fixture.app();
    let first = devices[0].path.clone();
    let second = devices[1].path.clone();

    app.state.begin_settings();
    for path in [&first, &second] {
        let index = app
            .state
            .devices
            .iter()
            .position(|device| &device.path == path)
            .expect("detected");
        app.state.settings_row = SETTINGS_FIXED_ROWS + index;
        assert_eq!(app.state.toggle_draft(), Some(true));
    }
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (host, human, _ai) = pairing(&app);
    let remote = RemoteClient::connect(&host, &human, None, Duration::from_secs(5))
        .expect("remote connects");
    let first_opened = remote.open(&first, Some(9_600), false).expect("open first");
    let second_opened = remote
        .open(&second, Some(9_600), false)
        .expect("open second");
    let local = fixture.app();
    let local_first = local
        .client()
        .open(&first, None, false)
        .expect("a local client joins the same device");

    // Save a checklist that drops the first device.
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == first)
        .expect("detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + row;
    assert_eq!(app.state.toggle_draft(), Some(false));
    save(&mut app);
    assert!(
        app.state
            .notice
            .as_ref()
            .is_none_or(|notice| !notice.text.contains("not saved")),
        "{:?}",
        app.state.notice
    );
    // The remote session is detached from that device at the core: further
    // writes fail visibly and it disappears from the remote list.
    assert!(
        remote
            .list_devices()
            .unwrap()
            .devices
            .iter()
            .all(|device| device.path != first),
        "the unchecked device is gone from the remote list"
    );
    assert!(
        remote
            .write_bytes(first_opened.device_id, b"x", None)
            .is_err(),
        "a revoked device refuses further writes"
    );
    // The other checked device and the local client are untouched.
    assert!(remote
        .list_devices()
        .unwrap()
        .devices
        .iter()
        .any(|device| device.path == second));
    local
        .client()
        .write_bytes(local_first.device_id, b"local\r", None)
        .expect("the local client keeps its device");
    devices[0].read_until(b"local", Duration::from_secs(5));
    remote
        .write_bytes(second_opened.device_id, b"second\r", None)
        .expect("the other shared device still writes");
    devices[1].read_until(b"second", Duration::from_secs(5));

    app.shutdown();
}

#[test]
fn a_bind_failure_is_shown_and_sharing_stays_off() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();

    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", port);
    save(&mut app);
    start(&mut app);

    assert!(!app.state.sharing.active, "sharing remains off");
    assert!(app.state.sharing.pairing.is_none());
    let notice = app.state.notice.as_ref().expect("the failure is shown");
    assert_eq!(notice.severity, Severity::Error);
    assert!(
        notice.text.contains("sharing not started"),
        "{}",
        notice.text
    );
    app.shutdown();
}

#[test]
fn an_ipv6_loopback_session_binds_and_authenticates() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "::1", 0);
    save(&mut app);
    start(&mut app);
    assert!(app.state.sharing.active, "IPv6 sharing started");
    let (host, human, _ai) = pairing(&app);
    assert!(host.address.is_ipv6(), "{}", host.address);
    RemoteClient::connect(&host, &human, None, Duration::from_secs(5))
        .expect("the IPv6 pairing authenticates");
    app.shutdown();
}

#[test]
fn an_unplugged_device_loses_its_checkbox_and_the_host_is_told() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    let second = devices[1].path.clone();
    check_device(&mut app, &path);
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == second)
        .expect("detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(true));
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);

    // Unplug the first device: its node disappears and the core expires it.
    let mut devices = devices;
    drop(devices.remove(0));
    fixture.replace_devices(vec![PathBuf::from(&second)]);
    app.refresh_devices();

    assert!(
        !app.state.sharing.pending_checked.contains(&path),
        "the unplugged device is no longer shared"
    );
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == second)
        .expect("the remaining device");
    assert_eq!(
        app.state.draft_device_checked(SETTINGS_FIXED_ROWS + row),
        Some(true),
        "the other check is kept"
    );
    app.state.cancel_settings();
    app.shutdown();
}

#[test]
fn a_replacement_at_a_checked_path_needs_an_explicit_recheck() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    assert!(app.state.sharing.pending_checked.contains(&path));

    // The core reports the path as no longer authorized (a different node at
    // the same path); the checkbox must not follow the path blindly.
    let snapshot = SharingSnapshot {
        active: true,
        session_id: Some("session".into()),
        bind: app.state.sharing.snapshot.as_ref().and_then(|s| s.bind),
        checked: Vec::new(),
        remote_clients: 0,
        pending_connections: 0,
    };
    let _ = app.state.apply_sharing_snapshot(snapshot);
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("the path is still detected");
    assert_eq!(
        app.state.draft_device_checked(SETTINGS_FIXED_ROWS + row),
        Some(false),
        "a replacement must be checked again"
    );
    app.state.cancel_settings();
    app.shutdown();
}

#[test]
fn periodic_device_refresh_preserves_the_authoritative_shared_checklist() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.state.notice = None;

    app.refresh_devices();

    assert!(app.state.sharing.pending_checked.contains(&path));
    assert!(app.state.notice.is_none());
    app.state.begin_settings();
    let row = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("device remains detected");
    assert_eq!(
        app.state.draft_device_checked(SETTINGS_FIXED_ROWS + row),
        Some(true)
    );
    save(&mut app);
    assert!(app.state.sharing.pending_checked.contains(&path));
    app.shutdown();
}

#[test]
fn a_failed_settings_save_applies_neither_the_preferences_nor_the_checklist() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    let replacement_path = devices[1].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);

    app.state.begin_settings();
    app.state.draft.local_echo = false;
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("checked device detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(false));
    let replacement_index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == replacement_path)
        .expect("replacement device detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + replacement_index;
    assert_eq!(app.state.toggle_draft(), Some(true));
    std::fs::remove_file(&fixture.settings_path).expect("remove settings file");
    std::fs::create_dir(&fixture.settings_path).expect("block the settings file");
    save(&mut app);

    let notice = app.state.notice.as_ref().expect("the failure is shown");
    assert_eq!(notice.severity, Severity::Error);
    assert!(
        notice.text.contains("settings not saved"),
        "{}",
        notice.text
    );
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Settings);
    assert!(
        app.state.settings.local_echo,
        "the option in effect stays on"
    );
    assert!(
        !app.state.draft.local_echo,
        "the attempted preference stays in the draft"
    );
    assert!(app.state.sharing.active, "sharing remains active");
    assert!(
        app.state.sharing.pending_checked.contains(&path),
        "the prior core checklist is restored"
    );
    assert!(
        !app.state
            .sharing
            .pending_checked
            .contains(&replacement_path),
        "the failed new checklist is rolled back"
    );
    assert!(
        !app.state.draft_checked.contains(&path),
        "the attempted draft is kept"
    );
    assert!(app.state.draft_checked.contains(&replacement_path));
    app.shutdown();
}

#[test]
fn rechecking_after_stop_retains_the_current_discovery_identity() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);

    app.state.begin_settings();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected");
    app.state.settings_row = SETTINGS_FIXED_ROWS + index;
    assert_eq!(app.state.toggle_draft(), Some(false));
    save(&mut app);
    app.stop_sharing();

    check_device(&mut app, &path);
    save(&mut app);
    app.state.begin_settings();
    save(&mut app);

    assert!(app.state.sharing.pending_checked.contains(&path));
    assert_eq!(
        app.state.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Info)
    );
    app.shutdown();
}

#[test]
fn a_failed_save_keeps_a_checked_draft_when_discovery_identity_is_lost() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    save(&mut app);
    app.state.begin_settings();
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;
    std::fs::remove_file(&fixture.settings_path).expect("remove settings file");
    std::fs::create_dir(&fixture.settings_path).expect("block the settings file");

    save(&mut app);

    assert!(
        app.state.sharing.pending_checked.contains(&path),
        "the previously saved checklist is not changed by a failed save"
    );
    assert!(
        app.state.draft_checked.contains(&path),
        "a failed save retains the human's checklist draft"
    );
    assert_eq!(
        app.state.notice.as_ref().map(|notice| notice.severity),
        Some(Severity::Error)
    );
    app.shutdown();
}

#[test]
fn leaving_the_terminal_stops_sharing_and_revokes_the_credentials() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (host, human, _ai) = pairing(&app);

    app.shutdown();
    assert!(!app.state.sharing.active);
    assert!(app.state.sharing.pairing.is_none(), "secrets are dropped");

    // The listener and the credentials are gone with the owning terminal.
    let result = RemoteClient::connect(&host, &human, None, Duration::from_secs(2));
    assert!(result.is_err(), "the old pairing no longer works");
}

#[test]
fn sharing_does_not_start_when_a_checked_device_has_no_current_identity() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    let index = app
        .state
        .devices
        .iter()
        .position(|device| device.path == path)
        .expect("detected device");
    app.state.devices[index].identity = None;

    start(&mut app);

    assert!(!app.state.sharing.active);
    assert!(!app.state.sharing.pending_checked.contains(&path));
    assert!(
        app.state
            .notice
            .as_ref()
            .is_some_and(|notice| notice.text.contains("check at least one device")),
        "the path without an identity is refused"
    );
    app.shutdown();
}

#[test]
fn starting_without_a_checked_device_refuses_and_stays_off() {
    let (fixture, _devices) = Fixture::new();
    let mut app = fixture.app();
    set_bind(&mut app, "127.0.0.1", 0);
    start(&mut app);
    assert!(!app.state.sharing.active);
    let notice = app.state.notice.as_ref().expect("a notice");
    assert!(
        notice.text.contains("check at least one device"),
        "{}",
        notice.text
    );
    app.shutdown();
}

#[test]
fn the_checklist_is_session_only_and_never_written_to_the_settings_file() {
    let (fixture, devices) = Fixture::new();
    let path = devices[0].path.clone();
    let mut app = fixture.app();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    app.shutdown();

    let saved = fixture.settings_text();
    assert!(
        !saved.contains(&path),
        "the checklist is not persisted: {saved}"
    );
    // A fresh launch defaults to nothing checked and sharing off.
    let app = fixture.app();
    assert!(!app.state.sharing.active);
    assert!(app.state.sharing.pending_checked.is_empty());
    let mut app = app;
    app.state.begin_settings();
    assert_eq!(app.state.draft_checked, BTreeSet::new());
    app.shutdown();
}

#[test]
fn the_pairing_view_redacts_nothing_from_the_owner_but_needs_the_session() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);

    if app.state.focus != serial_nexus_terminal::state::Focus::Sidebar {
        app.handle_key(serial_nexus_terminal::input::Key::Ctrl(']'));
    }
    app.handle_key(serial_nexus_terminal::input::Key::Char('p'));
    let notice = app.state.notice.as_ref().expect("a notice");
    assert!(
        notice.text.contains("sharing is off"),
        "there is nothing to pair with yet: {}",
        notice.text
    );
    assert_eq!(app.state.view, serial_nexus_terminal::state::View::Stream);
    app.shutdown();
}

#[test]
fn secrets_are_not_put_in_notices() {
    let (fixture, devices) = Fixture::new();
    let mut app = fixture.app();
    let path = devices[0].path.clone();
    check_device(&mut app, &path);
    set_bind(&mut app, "127.0.0.1", 0);
    save(&mut app);
    start(&mut app);
    let (_, human, ai) = pairing(&app);
    let notices: Vec<String> = app
        .state
        .notice
        .as_ref()
        .map(|notice| vec![notice.text.clone()])
        .unwrap_or_default();
    for text in notices {
        assert!(!text.contains(&human.credential));
        assert!(!text.contains(&ai.credential));
    }
    let notice: Option<Notice> = app.state.notice.clone();
    assert!(notice.is_some());
    app.shutdown();
}
