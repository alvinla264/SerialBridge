//! The remote-connect flow in the real terminal: the Connect button's form,
//! an authenticated pair, an allowed unopened port at a chosen baud, and the
//! endpoint grouping in the sidebar.

mod support;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serial_nexus_client::{ClientInfo, ShareBind};
use serial_nexus_protocol::ShareSessionInfo;
use support::tui::{TuiSession, KEY_CTRL_BRACKET, KEY_ENTER, KEY_TAB};
use support::{connect, start_core, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

const ROWS: u16 = 32;
const COLS: u16 = 110;

static REMOTE_TESTS: Mutex<()> = Mutex::new(());

fn serial_test() -> MutexGuard<'static, ()> {
    REMOTE_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

struct Fixture {
    _local: TempDir,
    _host: TempDir,
    _xdg: TempDir,
    xdg: PathBuf,
    local_socket: PathBuf,
    device: SimulatedDevice,
    info: ShareSessionInfo,
    _lister: PortListerGuard,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let guard = serial_test();
        let (local, local_socket) =
            start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let (host_dir, host_socket) =
            start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let device = SimulatedDevice::new();
        let lister = PortListerGuard::from_path(device.path.clone());
        let owner = connect(&host_socket, ClientInfo::human("host-terminal"));
        let info = owner
            .start_sharing(
                ShareBind::new("127.0.0.1".parse().unwrap(), 0),
                vec![device.path.clone()],
            )
            .expect("the host shares the device");
        // Keep the owning connection alive for the whole test.
        std::mem::forget(owner);
        let xdg = TempDir::new().expect("xdg");
        Self {
            _local: local,
            _host: host_dir,
            local_socket,
            xdg: xdg.path().to_path_buf(),
            _xdg: xdg,
            device,
            info,
            _lister: lister,
            _guard: guard,
        }
    }

    fn tui(&self) -> TuiSession {
        TuiSession::spawn(&self.local_socket, &self.xdg, ROWS, COLS)
    }
}

/// Move the keyboard focus until the header names `want`. A single Ctrl+]
/// toggles; under load the event can take a moment, so the key is repeated
/// only after a generous wait, never in a tight burst.
fn focus(tui: &mut TuiSession, want: &str) {
    for _ in 0..3 {
        tui.send(KEY_CTRL_BRACKET);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if tui.text().contains(want) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    panic!("focus never became {want:?}:\n{}", tui.text());
}

fn click_text(tui: &mut TuiSession, needle: &str) {
    let (column, row) = tui
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", tui.text()));
    tui.click(column, row);
}

#[test]
fn the_connect_form_pairs_with_a_host_and_opens_its_allowed_unopened_port() {
    let mut fixture = Fixture::new();
    let path = fixture.device.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    click_text(&mut tui, "[ Connect ]");
    tui.wait_for_text("focus: connect");
    tui.wait_for_text("Host address");
    tui.send_text(&fixture.info.host.address.ip().to_string());
    tui.send(KEY_TAB);
    tui.send_text(&fixture.info.host.address.port().to_string());
    tui.send(KEY_TAB);
    tui.send_text(&fixture.info.host.ca_cert_der_b64);
    tui.send(KEY_TAB);
    tui.send_text(&fixture.info.host.host_cert_fingerprint_sha256);
    tui.send(KEY_TAB);
    tui.send_text(&fixture.info.human.credential);
    tui.send(KEY_ENTER);

    tui.wait_for_text("connected to");
    tui.wait_for_text("Remote 127.0.0.1:");
    tui.wait_for_text(&path);
    // The group heading names the endpoint.
    assert!(
        tui.text().contains("Remote 127.0.0.1:"),
        "the remote group is labelled:\n{}",
        tui.text()
    );

    // The device is allowed but unopened: Enter asks for a baud rate first.
    // The local core also lists the path (the test port lister is
    // process-wide), so pick the row after the remote group heading.
    let group_row = (0..tui.rows())
        .find(|row| tui.row(*row).contains("Remote 127.0.0.1:"))
        .expect("the remote group heading");
    let row = (group_row + 1..tui.rows())
        .find(|row| tui.row(*row).contains(&path))
        .unwrap_or_else(|| {
            panic!(
                "the remote device row:
{}",
                tui.text()
            )
        });
    let column = tui.row(row).find(&path).expect("the path in its row") as u16;
    tui.click(column, row);
    tui.send(KEY_ENTER);
    tui.wait_for_text("Baud for");
    tui.send_text("9600");
    tui.send(KEY_ENTER);
    tui.wait_until("the remote device shown", |text| {
        text.lines()
            .next()
            .is_some_and(|header| header.contains(&path))
    });

    // Typed input reaches the paired host's device.
    focus(&mut tui, "focus: sidebar");
    focus(&mut tui, "focus: stream");
    tui.send_text("ping");
    fixture.device.read_until(b"ping", Duration::from_secs(5));

    // Narrow resize keeps the remote stream usable.
    tui.resize(20, 40);
    tui.wait_until("the narrow stream", |text| {
        text.contains("ping") || text.contains(&path)
    });
    tui.resize(ROWS, COLS);
    tui.wait_until("both panes again", |text| text.contains('│'));

    focus(&mut tui, "focus: sidebar");
    tui.quit();
}
