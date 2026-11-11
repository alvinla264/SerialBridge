//! The remote half of the terminal against real cores and TLS: connecting
//! with pairing information, opening an allowed unopened port, two hosts
//! coexisting, and a disconnect that leaves the other sessions alone.

mod support;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serial_nexus_client::{Client, ClientInfo, ShareBind};
use serial_nexus_core::enumerate::{install_port_lister, PortInfo};
use serial_nexus_protocol::ShareSessionInfo;
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::Key;
use serial_nexus_terminal::settings::SettingsStore;
use serial_nexus_terminal::state::{EndpointId, View};
use support::{connect, start_core, SimulatedDevice};
use tempfile::TempDir;

static REMOTE_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    REMOTE_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// A second core on its own socket, sharing the given PTY paths.
struct Host {
    _core: TempDir,
    _xdg: TempDir,
    owner: Client,
    info: ShareSessionInfo,
}

fn start_host(devices: &[&str], lister_paths: Arc<Mutex<Vec<PathBuf>>>) -> Host {
    let (core, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let paths = Arc::clone(&lister_paths);
    install_port_lister(Some(Arc::new(move || {
        paths
            .lock()
            .expect("paths")
            .iter()
            .map(|path| PortInfo {
                path: path.to_string_lossy().into_owned(),
                description: Some("simulated".into()),
            })
            .collect()
    })));
    let owner = connect(&socket, ClientInfo::human("host-terminal"));
    let info = owner
        .start_sharing(
            ShareBind::new("127.0.0.1".parse().unwrap(), 0),
            devices.iter().map(|path| (*path).to_string()).collect(),
        )
        .expect("sharing starts");
    Host {
        _core: core,
        _xdg: TempDir::new().expect("xdg"),
        owner,
        info,
    }
}

struct Terminal {
    _xdg: TempDir,
    settings_path: PathBuf,
    app: App,
}

impl Terminal {
    fn new(socket: &PathBuf) -> Self {
        let xdg = TempDir::new().expect("xdg");
        let settings_path = xdg.path().join("serial-nexus/terminal.json");
        let settings = SettingsStore::at(&settings_path)
            .load_or_create()
            .expect("settings");
        let client = connect(socket, ClientInfo::human("remote-terminal"));
        let app = App::with_preferences(
            client,
            40,
            100,
            settings,
            Some(SettingsStore::at(&settings_path)),
        );
        Self {
            _xdg: xdg,
            settings_path,
            app,
        }
    }

    fn settings_text(&self) -> String {
        std::fs::read_to_string(&self.settings_path).unwrap_or_default()
    }

    fn connect(&mut self, host: &ShareSessionInfo) {
        self.app.start_connect();
        let form = self.app.state.connect.as_mut().expect("form");
        form.address = host.host.address.ip().to_string();
        form.port = host.host.address.port().to_string();
        form.ca = host.host.ca_cert_der_b64.clone();
        form.fingerprint = host.host.host_cert_fingerprint_sha256.clone();
        form.credential = host.human.credential.clone();
        self.app.submit_connect();
    }

    /// The remote endpoint that shows `path`. The test port lister is
    /// process-wide, so the local core also lists the path as a detected
    /// (closed) device; only the non-local row is the paired host's.
    fn endpoint_of(&self, path: &str) -> EndpointId {
        self.app
            .state
            .devices
            .iter()
            .find(|device| {
                device.path == path
                    && device.endpoint != serial_nexus_terminal::state::LOCAL_ENDPOINT
            })
            .unwrap_or_else(|| panic!("{path} is not a remote device"))
            .endpoint
    }
}

#[test]
fn a_paired_terminal_opens_an_allowed_unopened_port_and_keeps_the_secret_out_of_the_settings_file()
{
    let _guard = serial_test();
    let local = TempDir::new().expect("local core");
    let local_socket = local.path().join("local.sock");
    let local_config = serial_nexus_core::CoreConfig {
        socket: local_socket.clone(),
        ..Default::default()
    };
    std::thread::spawn(move || {
        let _ = serial_nexus_core::server::run(local_config);
    });
    support::wait_until(Duration::from_secs(5), || {
        std::os::unix::net::UnixStream::connect(&local_socket).is_ok()
    });

    let mut device = SimulatedDevice::new();
    let path = device.path.clone();
    let paths = Arc::new(Mutex::new(vec![PathBuf::from(&path)]));
    let host = start_host(&[&path], paths);

    let mut terminal = Terminal::new(&local_socket);
    terminal.connect(&host.info);
    assert_eq!(
        terminal.app.state.view,
        View::Stream,
        "a successful connect returns to the stream"
    );
    let endpoint = terminal.endpoint_of(&path);
    assert_eq!(
        terminal.app.state.endpoint_label(endpoint),
        format!("Remote {}", host.info.bind)
    );
    let groups: Vec<String> = terminal
        .app
        .state
        .sidebar_rows()
        .iter()
        .filter_map(|row| match row {
            serial_nexus_terminal::state::SidebarRow::Group(endpoint) => {
                Some(terminal.app.state.endpoint_label(*endpoint))
            }
            _ => None,
        })
        .collect();
    assert!(
        groups.contains(&format!("Remote {}", host.info.bind)),
        "{groups:?}"
    );

    // Opening the allowed but never-opened port asks for (and uses) a baud.
    let key = terminal
        .app
        .open_endpoint(endpoint, &path, Some(9_600))
        .expect("open the remote device");
    assert_eq!(terminal.app.state.shown_key(), Some(key));
    terminal.app.write_shown(b"hello", Key::Char('h'));
    device.read_until(b"hello", Duration::from_secs(5));

    // The pairing values never reach the settings file.
    let saved = terminal.settings_text();
    assert!(!saved.contains(&host.info.human.credential), "{saved}");
    assert!(!saved.contains("credential"));
    assert!(!saved.contains(&host.info.host.ca_cert_der_b64));

    terminal.app.shutdown();
}

#[test]
fn a_wrong_host_certificate_is_refused_and_leaves_no_endpoint() {
    let _guard = serial_test();
    let (local, local_socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let device = SimulatedDevice::new();
    let paths = Arc::new(Mutex::new(vec![PathBuf::from(&device.path)]));
    let host = start_host(&[&device.path], paths);

    let mut terminal = Terminal::new(&local_socket);
    terminal.app.start_connect();
    let form = terminal.app.state.connect.as_mut().expect("form");
    form.address = host.info.host.address.ip().to_string();
    form.port = host.info.host.address.port().to_string();
    form.ca = host.info.host.ca_cert_der_b64.clone();
    form.fingerprint = "00".repeat(32);
    form.credential = host.info.human.credential.clone();
    terminal.app.submit_connect();

    assert_eq!(
        terminal.app.state.view,
        View::Connect,
        "the form stays open after a rejected host"
    );
    let error = terminal
        .app
        .state
        .connect
        .as_ref()
        .and_then(|form| form.error.clone())
        .expect("the refusal is shown");
    assert!(error.contains("not connected"), "{error}");
    assert!(!error.contains(&host.info.human.credential), "{error}");
    assert!(
        !terminal
            .app
            .state
            .devices
            .iter()
            .any(|device| device.endpoint != serial_nexus_terminal::state::LOCAL_ENDPOINT),
        "no remote device was added"
    );
    drop(local);
    terminal.app.shutdown();
}

#[test]
fn two_hosts_and_the_local_core_coexist_even_with_equal_device_ids() {
    let _guard = serial_test();
    let (_local_core, local_socket) =
        start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let mut local_device = SimulatedDevice::new();
    let local_seed = connect(&local_socket, ClientInfo::human("local-seed"));
    local_seed
        .open(&local_device.path, Some(9_600), false)
        .expect("open local device");

    let mut remote_one = SimulatedDevice::new();
    let mut remote_two = SimulatedDevice::new();
    let first_path = remote_one.path.clone();
    let second_path = remote_two.path.clone();

    let paths = Arc::new(Mutex::new(vec![
        PathBuf::from(&first_path),
        PathBuf::from(&second_path),
    ]));
    let first_host = start_host(&[&first_path], Arc::clone(&paths));
    let second_host = start_host(&[&second_path], Arc::clone(&paths));
    let mut terminal = Terminal::new(&local_socket);
    terminal.connect(&first_host.info);
    terminal.connect(&second_host.info);
    let first_endpoint = terminal.endpoint_of(&first_path);
    let second_endpoint = terminal.endpoint_of(&second_path);
    assert_ne!(first_endpoint, second_endpoint, "two different hosts");

    let first_key = terminal
        .app
        .open_endpoint(first_endpoint, &first_path, Some(9_600))
        .expect("open first remote");
    let second_key = terminal
        .app
        .open_endpoint(second_endpoint, &second_path, Some(19_200))
        .expect("open second remote");
    assert_eq!(
        first_key.device_id, second_key.device_id,
        "both cores number devices from one"
    );
    assert_ne!(first_key, second_key, "the endpoint keeps them apart");
    assert_eq!(terminal.app.state.shown_key(), Some(second_key));

    // Typing goes only to the shown remote device, even when the highlight is
    // moved to another endpoint's row.
    let local_index = terminal
        .app
        .state
        .devices
        .iter()
        .position(|device| device.endpoint == serial_nexus_terminal::state::LOCAL_ENDPOINT)
        .expect("local row");
    terminal.app.state.select_index(local_index);
    terminal.app.write_shown(b"R", Key::Char('R'));
    remote_two.read_until(b"R", Duration::from_secs(5));
    assert!(
        remote_one.try_read().is_empty(),
        "the other remote device got nothing"
    );

    // The local stream still works.
    let local_key = terminal
        .app
        .state
        .devices
        .iter()
        .find(|device| device.endpoint == serial_nexus_terminal::state::LOCAL_ENDPOINT)
        .map(|device| device.path.clone())
        .expect("local path");
    let key = terminal
        .app
        .open_endpoint(
            serial_nexus_terminal::state::LOCAL_ENDPOINT,
            &local_key,
            None,
        )
        .expect("join local");
    terminal.app.write_shown(b"L", Key::Char('L'));
    local_device.read_until(b"L", Duration::from_secs(5));

    assert_eq!(terminal.app.state.shown_key(), Some(key));

    terminal.app.shutdown();
}

#[test]
fn unchecking_one_remote_device_keeps_the_other_and_the_local_core() {
    let _guard = serial_test();
    let (_local_core, local_socket) =
        start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let local_device = SimulatedDevice::new();
    let local_seed = connect(&local_socket, ClientInfo::human("local-seed"));
    local_seed
        .open(&local_device.path, Some(9_600), false)
        .expect("open local");

    let mut first = SimulatedDevice::new();
    let mut second = SimulatedDevice::new();
    let first_path = first.path.clone();
    let second_path = second.path.clone();
    let paths = Arc::new(Mutex::new(vec![
        PathBuf::from(&first_path),
        PathBuf::from(&second_path),
    ]));
    let host = start_host(&[&first_path, &second_path], paths);
    let mut terminal = Terminal::new(&local_socket);
    terminal.connect(&host.info);
    let first_endpoint = terminal.endpoint_of(&first_path);
    let second_endpoint = terminal.endpoint_of(&second_path);
    let first_key = terminal
        .app
        .open_endpoint(first_endpoint, &first_path, Some(9_600))
        .expect("open first");
    let second_key = terminal
        .app
        .open_endpoint(second_endpoint, &second_path, Some(9_600))
        .expect("open second");

    // The host revokes the first device. The terminal sees it at the next
    // refresh and its session stops being writable; the second stays.
    host.owner
        .uncheck_shared_devices(vec![first_path.clone()])
        .expect("uncheck");
    terminal.app.refresh_devices();

    terminal.app.show_session_now(second_key);
    terminal.app.write_shown(b"still", Key::Char('s'));
    second.read_until(b"still", Duration::from_secs(5));
    assert!(
        first.try_read().is_empty(),
        "the revoked device got no bytes"
    );

    // A write to the revoked session fails visibly and is not retried.
    terminal.app.show_session_now(first_key);
    terminal.app.write_shown(b"never", Key::Char('n'));
    let notice = terminal.app.state.notice.as_ref().expect("notice");
    assert!(
        notice.text.contains("write to"),
        "the failure is visible: {}",
        notice.text
    );
    assert!(
        first.try_read().is_empty(),
        "the failed write was not replayed"
    );

    terminal.app.shutdown();
}

#[test]
fn a_remote_host_stopping_sharing_removes_only_its_own_sessions() {
    let _guard = serial_test();
    let (_local_core, local_socket) =
        start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
    let local_device = SimulatedDevice::new();
    let local_seed = connect(&local_socket, ClientInfo::human("local-seed"));
    local_seed
        .open(&local_device.path, Some(9_600), false)
        .expect("open local");

    let remote_device = SimulatedDevice::new();
    let remote_path = remote_device.path.clone();
    let paths = Arc::new(Mutex::new(vec![PathBuf::from(&remote_path)]));
    let host = start_host(&[&remote_path], paths);
    let mut terminal = Terminal::new(&local_socket);
    terminal.connect(&host.info);
    let endpoint = terminal.endpoint_of(&remote_path);
    terminal
        .app
        .open_endpoint(endpoint, &remote_path, Some(9_600))
        .expect("open remote");

    host.owner.stop_sharing().expect("stop sharing");
    terminal.app.refresh_devices();

    assert!(
        terminal.app.state.remote_labels.is_empty(),
        "the remote group is gone"
    );
    assert!(
        terminal
            .app
            .state
            .devices
            .iter()
            .all(|device| device.endpoint == serial_nexus_terminal::state::LOCAL_ENDPOINT),
        "only local rows remain"
    );
    assert!(
        terminal.app.state.sessions.is_empty(),
        "only the remote session existed, and it is gone"
    );
    // The local core still works from this terminal.
    let local_path = local_device.path.clone();
    terminal
        .app
        .open_endpoint(
            serial_nexus_terminal::state::LOCAL_ENDPOINT,
            &local_path,
            None,
        )
        .expect("the local device still opens");
    assert!(terminal
        .app
        .state
        .sessions
        .values()
        .all(|session| session.endpoint == serial_nexus_terminal::state::LOCAL_ENDPOINT));

    terminal.app.shutdown();
}
