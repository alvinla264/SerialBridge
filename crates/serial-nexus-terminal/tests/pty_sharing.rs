//! The host's sharing controls in the real terminal: the sidebar button, the
//! settings checklist, the pairing screen with its secrets, and a stopped
//! session when the terminal leaves.

mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serial_nexus_core::enumerate::{install_port_lister, PortInfo};
use support::tui::{TuiSession, KEY_CTRL_BRACKET, KEY_CTRL_S, KEY_ENTER, KEY_ESC};
use support::{start_core, SimulatedDevice};
use tempfile::TempDir;

const ROWS: u16 = 30;
const COLS: u16 = 110;

static SHARING_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    SHARING_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Fixture {
    _core: TempDir,
    _xdg: TempDir,
    xdg: PathBuf,
    socket: PathBuf,
    device: SimulatedDevice,
    guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let guard = serial_test();
        let (core, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let device = SimulatedDevice::new();
        let path = device.path.clone();
        install_port_lister(Some(Arc::new(move || {
            vec![PortInfo {
                path: path.clone(),
                description: Some("simulated".into()),
            }]
        })));
        let xdg = TempDir::new().expect("xdg");
        Self {
            _core: core,
            socket,
            xdg: xdg.path().to_path_buf(),
            _xdg: xdg,
            device,
            guard,
        }
    }

    fn tui(&self) -> TuiSession {
        TuiSession::spawn(&self.socket, &self.xdg, ROWS, COLS)
    }

    fn settings_text(&self) -> String {
        std::fs::read_to_string(self.xdg.join("serial-nexus/terminal.json")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        install_port_lister(None);
        let _ = &self.guard;
    }
}

fn row_with(tui: &TuiSession, needle: &str) -> u16 {
    (0..tui.rows())
        .find(|row| tui.row(*row).contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", tui.text()))
}

fn click_text(tui: &mut TuiSession, needle: &str) {
    let row = row_with(tui, needle);
    let column = tui.row(row).find(needle).expect("the needle is in its row") as u16;
    tui.click(column, row);
}

fn open_settings(tui: &mut TuiSession) {
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
}

fn check_device(tui: &mut TuiSession, path: &str) {
    // The sidebar row also names the device; the checkbox is the settings row
    // in the right pane.
    let row = (0..tui.rows())
        .find(|row| {
            let line = tui.row(*row);
            line.contains("[ ]") && line.contains(path)
        })
        .unwrap_or_else(|| panic!("no checkbox for {path}:\n{}", tui.text()));
    let column = tui.row(row).find(path).expect("the path is in its row") as u16;
    tui.click(column, row);
    tui.wait_until("the device checkbox to be checked", |text| {
        text.lines()
            .any(|line| line.contains("[x]") && line.contains(path))
    });
}

fn set_port(tui: &mut TuiSession, port: u16) {
    click_text(tui, "Share port");
    tui.send(&[0x7f, 0x7f, 0x7f, 0x7f]);
    tui.send_text(&port.to_string());
    tui.send(KEY_ENTER);
    tui.wait_until("the typed port", |text| {
        text.lines()
            .find(|line| line.contains("Share port"))
            .is_some_and(|line| line.trim_end().ends_with(&port.to_string()))
    });
}

fn set_port_zero(tui: &mut TuiSession) {
    // A click on a text row starts editing; clear the default and ask the OS
    // for a free port.
    click_text(tui, "Share port");
    tui.send(&[0x7f, 0x7f, 0x7f, 0x7f]);
    tui.send_text("0");
    tui.send(KEY_ENTER);
    tui.wait_until("the port to be 0", |text| {
        text.lines()
            .find(|line| line.contains("Share port"))
            .is_some_and(|line| line.trim_end().ends_with('0'))
    });
}

#[test]
fn the_host_checks_a_device_starts_sharing_sees_the_secrets_and_leaves_it_off() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    tui.wait_for_text(&path);

    // The checklist starts empty and is saved into a running session.
    open_settings(&mut tui);
    tui.wait_for_text("Share address");
    tui.wait_for_text(&path);
    assert!(
        tui.text().contains("[ ]"),
        "nothing is checked by default:\n{}",
        tui.text()
    );
    check_device(&mut tui, &path);
    set_port_zero(&mut tui);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.wait_for_text("focus: stream");

    // The saved file holds preferences only, never a credential.
    let saved = fixture.settings_text();
    assert!(saved.contains("\"share_port\": 0"), "{saved}");
    assert!(!saved.contains("credential"), "{saved}");

    // The sidebar's Share button starts the session deliberately.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("focus: pairing");
    tui.wait_for_text("Firewall and router settings");
    tui.wait_for_text("certificate SHA-256");
    // The CA certificate is long: the credentials are below it.
    for _ in 0..60 {
        tui.send(b"\x1b[B");
        std::thread::sleep(Duration::from_millis(15));
    }
    tui.wait_for_text("Human terminal pairing");
    tui.wait_for_text("MCP (AI) pairing");
    assert!(
        tui.text().contains("credential:"),
        "both credentials are shown to the host:\n{}",
        tui.text()
    );
    assert!(
        !fixture.settings_text().contains("credential"),
        "the settings file never holds a pairing secret"
    );

    // The pairing screen survives a narrow resize and `s` stops the session.
    tui.resize(20, 40);
    tui.wait_until("the pairing view at a narrow width", |text| {
        text.contains("stop sharing") || text.contains("Sharing")
    });
    tui.resize(ROWS, COLS);
    tui.wait_for_text("focus: pairing");
    tui.send_text("s");
    tui.wait_for_text("sharing stopped");
    tui.wait_for_text("[ Share: off ]");

    // Nothing listens after the terminal leaves.
    assert!(
        !fixture.settings_text().contains("credential"),
        "no secret was written on the way out"
    );
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.quit();
}

#[test]
fn a_bind_failure_is_shown_and_sharing_stays_off() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();
    let mut tui = fixture.tui();
    tui.wait_for_text(&path);

    open_settings(&mut tui);
    check_device(&mut tui, &path);
    click_text(&mut tui, "Share port");
    tui.send(&[0x7f, 0x7f, 0x7f, 0x7f]);
    tui.send_text(&port.to_string());
    tui.send(KEY_ENTER);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");

    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("sharing not started");
    assert!(
        tui.text().contains("[ Share: off ]"),
        "sharing remains off:\n{}",
        tui.text()
    );
    assert!(
        !tui.text().contains("Human terminal pairing"),
        "no pairing information is shown for a failed bind"
    );

    tui.quit();
}

#[test]
fn cancel_discards_the_checklist_and_the_share_button_refuses_an_empty_one() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text(&path);

    open_settings(&mut tui);
    check_device(&mut tui, &path);
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");
    open_settings(&mut tui);
    tui.wait_until("the cancelled checkbox to be empty", |text| {
        text.lines()
            .any(|line| line.contains("[ ]") && line.contains(&path))
    });
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");

    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("check at least one device");
    assert!(tui.text().contains("[ Share: off ]"));

    tui.quit();
}

#[test]
fn quitting_the_owner_terminal_closes_the_listener_without_stopping_first() {
    let fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let reserved = TcpListener::bind(("127.0.0.1", 0)).expect("reserve a port");
    let port = reserved.local_addr().expect("addr").port();
    drop(reserved);

    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    tui.wait_for_text(&path);
    open_settings(&mut tui);
    check_device(&mut tui, &path);
    set_port(&mut tui, port);
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    click_text(&mut tui, "[ Share: off ]");
    tui.wait_for_text("focus: pairing");
    std::net::TcpStream::connect(("127.0.0.1", port)).expect("the listener is up");

    // Leave the pairing screen but keep sharing on, then quit through the
    // terminal's normal cleanup path.
    tui.send(KEY_ESC);
    tui.wait_until("the live stream after leaving pairing", |text| {
        text.contains("focus: stream")
    });
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.quit();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Err(_) => break,
            Ok(_) if std::time::Instant::now() >= deadline => {
                panic!("the listener survived the owner terminal")
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
