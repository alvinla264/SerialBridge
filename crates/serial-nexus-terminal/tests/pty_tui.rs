//! The real terminal in a PTY: two simulated serial devices behind a real
//! core, driven through the actual crossterm input path and checked on the
//! actual drawn screen.
//!
//! These tests exercise what the app-level tests cannot: raw mode, mouse
//! reporting, SIGWINCH-driven resize, alternate-screen drawing and the key
//! decoding a terminal really sends.

mod support;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use serial_nexus_client::{Client, ClientInfo};
use support::tui::{
    TuiSession, KEY_CTRL_BRACKET, KEY_CTRL_Q, KEY_CTRL_S, KEY_DOWN, KEY_ENTER, KEY_ESC,
    KEY_PAGE_UP, KEY_TAB, KEY_UP, STEP,
};
use support::{connect, start_core, SimulatedDevice};
use tempfile::TempDir;

const ROWS: u16 = 24;
const COLS: u16 = 100;

/// Two open devices on a real core, owned by a seed client so the terminal can
/// join them; plain PTYs are not enumerated, so an already-open device is how
/// the sidebar learns about them.
struct Fixture {
    _core: TempDir,
    socket: PathBuf,
    xdg: TempDir,
    first: SimulatedDevice,
    second: SimulatedDevice,
    seed: Client,
}

impl Fixture {
    fn new() -> Self {
        let (core, socket) = start_core(Duration::from_secs(5), Duration::from_secs(300), 500);
        let first = SimulatedDevice::new();
        let second = SimulatedDevice::new();
        let seed = connect(&socket, ClientInfo::human("pty-seed"));
        seed.open(&first.path, Some(9_600), false)
            .expect("open first");
        seed.open(&second.path, Some(19_200), false)
            .expect("open second");
        Self {
            _core: core,
            socket,
            xdg: TempDir::new().expect("xdg"),
            first,
            second,
            seed,
        }
    }

    fn tui(&self) -> TuiSession {
        TuiSession::spawn(&self.socket, self.xdg.path(), ROWS, COLS)
    }

    fn settings_file(&self) -> PathBuf {
        self.xdg.path().join("serial-nexus/terminal.json")
    }

    fn settings(&self) -> String {
        fs::read_to_string(self.settings_file()).unwrap_or_default()
    }
}

/// The screen row of a device in the sidebar's list.
fn device_row(tui: &TuiSession, path: &str) -> u16 {
    (1..tui.rows() - 2)
        .find(|row| tui.row(*row).contains(path))
        .unwrap_or_else(|| panic!("{path} is not in the sidebar:\n{}", tui.text()))
}

/// Click a device row and press Enter: open or join it.
fn open_device(tui: &mut TuiSession, path: &str) {
    let row = device_row(tui, path);
    tui.click(2, row);
    tui.send(KEY_ENTER);
}

fn shown_path(tui: &TuiSession) -> String {
    tui.row(0)
}

fn wait_for_shown(tui: &mut TuiSession, path: &str) {
    tui.wait_until(&format!("the header to show {path}"), |text| {
        text.lines()
            .next()
            .is_some_and(|header| header.contains(path))
    });
}

fn settings_row(tui: &TuiSession, name: &str) -> u16 {
    (1..tui.rows() - 1)
        .find(|row| tui.row(*row).contains(name))
        .unwrap_or_else(|| panic!("{name} is not on screen:\n{}", tui.text()))
}

/// The right pane's part of a row, after the separator.
fn stream_row(tui: &TuiSession, row: u16) -> String {
    let text = tui.row(row);
    match text.split_once('│') {
        Some((_, right)) => right.trim_start().trim_end().to_string(),
        None => text.trim_end().to_string(),
    }
}

/// Wait for a settings flag to show `value`; the screen redraws on the event
/// loop's next turn, so a toggle is never asserted synchronously.
fn wait_flag(tui: &mut TuiSession, name: &str, value: &str) {
    tui.wait_until(&format!("{name} to be {value}"), |text| {
        text.lines()
            .find(|line| line.contains(name))
            .and_then(|line| line.rsplit(' ').next())
            .is_some_and(|shown| shown == value)
    });
}

fn flag_is(tui: &TuiSession, name: &str, value: &str) -> bool {
    let row = settings_row(tui, name);
    let text = tui.row(row);
    let shown = text.rsplit(' ').next().unwrap_or_default();
    shown == value
}

fn click_button(tui: &mut TuiSession, label: &str) {
    let (column, row) = tui
        .find(label)
        .unwrap_or_else(|| panic!("{label} is not on screen:\n{}", tui.text()));
    tui.click(column, row);
}

fn wait_for_notice(tui: &mut TuiSession, needle: &str) {
    tui.wait_for_text(needle);
}

#[test]
fn two_devices_render_side_by_side_with_independent_streams() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();

    // The real binary draws the sidebar beside an empty right pane.
    tui.wait_for_text("Devices (");
    tui.wait_for_text(&first_path);
    tui.wait_for_text(&second_path);
    assert!(
        tui.text().contains('│'),
        "the separator between the panes is drawn:\n{}",
        tui.text()
    );
    assert!(
        tui.text().contains("Highlight a device on the left"),
        "the right pane is beside the sidebar:\n{}",
        tui.text()
    );

    // Opening the first device shows its stream alone.
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    tui.wait_for_text("focus: stream");
    fixture.first.write(b"alpha-one\r\n");
    tui.wait_for_text("alpha-one");
    assert!(!tui.text().contains("beta-one"), "{}", tui.text());

    // Ctrl+] focuses the sidebar without changing the shown stream, and the
    // second device opens beside it.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    assert!(shown_path(&tui).contains(&first_path));
    assert!(tui.row(device_row(&tui, &first_path)).contains('▸'));

    // Merely highlighting the other row keeps it apart from the shown one.
    let second_row_index = device_row(&tui, &second_path);
    tui.click(2, second_row_index);
    tui.wait_until("the second row to be highlighted", |text| {
        text.lines()
            .nth(second_row_index as usize)
            .is_some_and(|line| line.contains('›'))
    });
    assert!(tui.row(device_row(&tui, &first_path)).contains('▹'));

    tui.send(KEY_ENTER);
    wait_for_shown(&mut tui, &second_path);
    fixture.second.write(b"beta-one\r\n");
    tui.wait_for_text("beta-one");
    assert!(
        !tui.text().contains("alpha-one"),
        "the right pane shows only the shown device:\n{}",
        tui.text()
    );
    assert!(tui.row(device_row(&tui, &second_path)).contains('▸'));

    // Both sessions stay attached on the core.
    let devices = fixture.seed.list_devices().expect("list devices").devices;
    let open = devices.iter().filter(|device| device.open).count();
    assert!(open >= 2, "both devices stay open: {devices:?}");
}

#[test]
fn a_single_click_highlights_without_switching_or_redirecting_input() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");

    // One click on the other row only highlights it.
    let second_row = device_row(&tui, &second_path);
    tui.click(2, second_row);
    tui.wait_until("the second row to be highlighted", |text| {
        text.lines()
            .nth(second_row as usize)
            .is_some_and(|line| line.contains('›'))
    });
    assert!(
        shown_path(&tui).contains(&first_path),
        "a click never changes the shown device: {}",
        shown_path(&tui)
    );
    assert!(
        tui.row(device_row(&tui, &first_path)).contains('▹'),
        "the shown device keeps its marker:\n{}",
        tui.text()
    );

    // Keys still go to the shown device, not to the highlighted row.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: stream");
    tui.send_text("h");
    fixture.first.read_until(b"h", STEP);
    assert!(
        fixture.second.try_read().is_empty(),
        "the highlighted device received nothing"
    );
}

#[test]
fn enter_and_double_click_switch_and_input_reaches_only_the_shown_device() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);

    // Enter switches to the highlighted attached device without reopening it.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);
    tui.send_text("1");
    fixture.second.read_until(b"1", STEP);
    assert!(
        fixture.first.try_read().is_empty(),
        "input stayed in the stream"
    );

    // A double-click on the first row switches back.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    let first_row = device_row(&tui, &first_path);
    tui.double_click(2, first_row);
    wait_for_shown(&mut tui, &first_path);
    tui.send_text("2");
    fixture.first.read_until(b"2", STEP);
    assert!(
        fixture.second.try_read().is_empty(),
        "a switch never duplicates input"
    );
}

#[test]
fn two_devices_keep_their_own_scrollback() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    for line in 0..30 {
        fixture.first.write(format!("A{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("A29");

    // The first device's kept rows hold only its own output.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("focus: scrollback");
    tui.send(KEY_PAGE_UP);
    tui.wait_for_text("A00");
    assert!(!tui.text().contains("B00"), "{}", tui.text());

    // The second device's scrollback is its own, not a share of the first.
    tui.send(KEY_ESC);
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);
    for line in 0..30 {
        fixture.second.write(format!("B{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("B29");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("focus: scrollback");
    tui.send(KEY_PAGE_UP);
    tui.wait_for_text("B00");
    assert!(!tui.text().contains("A00"), "{}", tui.text());
}

#[test]
fn scrollback_off_clears_kept_rows_for_every_session() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    // Two sessions with identifiable rows already scrolled off the live view.
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    for line in 0..30 {
        fixture
            .first
            .write(format!("OLD-A{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("OLD-A29");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);
    for line in 0..30 {
        fixture
            .second
            .write(format!("OLD-B{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("OLD-B29");

    // Turn scrollback off; both sessions' kept rows are freed.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send(KEY_DOWN);
    tui.send_text(" ");
    wait_flag(&mut tui, "Scrollback", "off");
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.wait_for_text("focus: stream");

    // Output while off is not kept either: it only lives on the screen.
    for line in 0..30 {
        fixture
            .first
            .write(format!("OFF-A{line:02}\r\n").as_bytes());
        fixture
            .second
            .write(format!("OFF-B{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("OFF-B29");

    // Scrollback on again: the old rows must not come back.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send(KEY_DOWN);
    tui.send_text(" ");
    wait_flag(&mut tui, "Scrollback", "on");
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.wait_for_text("focus: stream");

    // The second session (shown) kept nothing from before or during the off
    // period beyond what still fits on its live screen.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("focus: scrollback");
    tui.send(KEY_PAGE_UP);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!tui.text().contains("OLD-B"), "{}", tui.text());
    assert!(!tui.text().contains("OFF-B00"), "{}", tui.text());
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");

    // The first session is the same: nothing was retained for it.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("focus: scrollback");
    tui.send(KEY_PAGE_UP);
    std::thread::sleep(Duration::from_millis(150));
    assert!(!tui.text().contains("OLD-A"), "{}", tui.text());
    assert!(!tui.text().contains("OFF-A00"), "{}", tui.text());
}

#[test]
fn ctrl_bracket_and_narrow_resize_keep_the_sessions_and_scrollback() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    // Both devices are joined by the terminal itself before the resize.
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    for line in 0..30 {
        fixture.first.write(format!("A{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("A29");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);
    for line in 0..30 {
        fixture.second.write(format!("B{line:02}\r\n").as_bytes());
    }
    tui.wait_for_text("B29");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);

    // The sidebar's `b` opens the kept rows; scroll back a little.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("focus: scrollback");
    for _ in 0..4 {
        tui.send(KEY_UP);
    }
    std::thread::sleep(Duration::from_millis(150));
    let top = stream_row(&tui, 1);
    assert!(top.contains("A"), "the top row is kept output: {top:?}");

    // Narrow: only the focused pane is drawn, and Ctrl+] switches which one.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.resize(24, 40);
    tui.wait_until("the narrow sidebar only", |text| {
        text.contains("Devices (") && !text.contains('│')
    });
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_until(&format!("the narrow right pane (top {top:?})"), |text| {
        !text.contains("Devices (")
            && text
                .lines()
                .nth(1)
                .is_some_and(|line| line.trim_end() == top)
    });
    assert_eq!(
        stream_row(&tui, 1),
        top,
        "the scrolled position survives the resize"
    );

    // Back at a usable width both panes return, still on the same rows.
    tui.resize(ROWS, COLS);
    tui.wait_until("both panes again", |text| text.contains('│'));
    assert_eq!(stream_row(&tui, 1), top, "the scroll position is unchanged");

    // Both sessions survived: each still has its own output and answers keys.
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");
    tui.send_text("k");
    fixture.first.read_until(b"k", STEP);
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);
    tui.wait_for_text("B29");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("focus: scrollback");
    tui.send(KEY_PAGE_UP);
    tui.wait_for_text("B00");
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");
    tui.send_text("s");
    fixture.second.read_until(b"s", STEP);
}

#[test]
fn settings_open_on_the_right_and_mouse_and_keyboard_save_and_cancel_work() {
    let fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);

    // The sidebar's Settings button opens the page in the right pane.
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.wait_for_text("Settings");
    assert!(
        tui.text().contains("Devices ("),
        "the sidebar stays visible"
    );
    assert!(flag_is(&tui, "Local echo", "on"));

    // A keyboard toggle, kept through a narrow resize and back.
    tui.send_text(" ");
    wait_flag(&mut tui, "Local echo", "off");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.resize(20, 40);
    tui.wait_until("the narrow sidebar", |text| {
        text.contains("Devices (") && !text.contains('│')
    });
    tui.resize(ROWS, COLS);
    tui.wait_until("both panes again", |text| text.contains('│'));
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: settings");
    wait_flag(&mut tui, "Local echo", "off");
    assert!(
        flag_is(&tui, "Local echo", "off"),
        "the draft survives a resize"
    );

    // Ctrl+S writes the file and returns to the stream.
    tui.send(KEY_CTRL_S);
    wait_for_notice(&mut tui, "settings saved");
    tui.wait_for_text("focus: stream");
    let saved = fixture.settings();
    assert!(saved.contains("\"local_echo\": false"), "{saved}");

    // Reopen, change scrollback, and Cancel with the mouse: nothing is saved.
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    wait_flag(&mut tui, "Local echo", "off");
    tui.send(KEY_DOWN);
    tui.send_text(" ");
    wait_flag(&mut tui, "Scrollback", "off");
    click_button(&mut tui, "[ Cancel ]");
    tui.wait_for_text("focus: stream");
    assert_eq!(fixture.settings(), saved, "Cancel keeps the file");

    // Reopen, turn local echo back on, and Save with the mouse.
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send_text(" ");
    wait_flag(&mut tui, "Local echo", "on");
    click_button(&mut tui, "[ Save ]");
    wait_for_notice(&mut tui, "settings saved");
    tui.wait_for_text("focus: stream");
    assert!(fixture.settings().contains("\"local_echo\": true"));
}

#[test]
fn a_failed_save_does_not_apply_and_a_dirty_draft_blocks_opening() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);

    // Make the settings path unwritable: a directory where the file was.
    let path = fixture.settings_file();
    fs::remove_file(&path).expect("remove settings file");
    fs::create_dir(&path).expect("block the settings file");

    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send_text(" ");
    wait_flag(&mut tui, "Local echo", "off");
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings not saved");
    assert!(
        tui.text().contains("focus: settings"),
        "a failed save stays on the page:\n{}",
        tui.text()
    );
    assert!(flag_is(&tui, "Local echo", "off"), "the draft is kept");

    // The option in effect did not change: typed keys are still echoed.
    tui.send(KEY_ESC);
    tui.wait_for_text("focus: stream");
    tui.send_text("ZZZ");
    fixture.first.read_until(b"ZZZ", STEP);
    tui.wait_for_text("ZZZ");

    // A dirty draft refuses opening a device and is kept.
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send_text(" ");
    wait_flag(&mut tui, "Local echo", "off");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    let row = device_row(&tui, &first_path);
    tui.click(2, row);
    tui.send(KEY_ENTER);
    tui.wait_for_text("unsaved changes");
    assert!(
        tui.text().contains("[ Save ]"),
        "the settings and the draft are still shown:\n{}",
        tui.text()
    );
    assert!(flag_is(&tui, "Local echo", "off"), "the draft is unchanged");
}

#[test]
fn explicit_disconnect_of_a_device_leaves_the_other_usable() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    tui.send(KEY_CTRL_BRACKET);
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);

    // Disconnect the highlighted (not shown) device from the sidebar.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    let first_row = device_row(&tui, &first_path);
    tui.click(2, first_row);
    tui.send_text("d");
    wait_for_notice(&mut tui, "disconnected");
    assert!(
        shown_path(&tui).contains(&second_path),
        "the other session stays shown: {}",
        shown_path(&tui)
    );

    // The remaining session still receives typed input.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: stream");
    tui.send_text("r");
    fixture.second.read_until(b"r", STEP);
}

#[test]
fn an_unplug_of_the_shown_device_hands_the_stream_to_the_other() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let second_path = fixture.second.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");

    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);
    tui.send(KEY_CTRL_BRACKET);
    open_device(&mut tui, &second_path);
    wait_for_shown(&mut tui, &second_path);

    // Pull the shown device: the core reports the close, and the other
    // session takes the right pane.
    let second = std::mem::replace(&mut fixture.second, SimulatedDevice::new());
    drop(second);
    wait_for_shown(&mut tui, &first_path);
    tui.wait_for_text("closed");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: stream");
    tui.send_text("u");
    fixture.first.read_until(b"u", STEP);
}

#[test]
fn stream_keys_ai_output_local_echo_and_scrollback_off_are_visible() {
    let mut fixture = Fixture::new();
    let first_path = fixture.first.path.clone();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    open_device(&mut tui, &first_path);
    wait_for_shown(&mut tui, &first_path);

    // The stream is a transparent terminal: Tab, arrows and Ctrl+Q reach the
    // device and do not quit or change focus.
    tui.send(KEY_TAB);
    fixture.first.read_until(b"\t", STEP);
    tui.send(KEY_UP);
    fixture.first.read_until(b"\x1b[A", STEP);
    tui.send(KEY_CTRL_Q);
    fixture.first.read_until(&[0x11], STEP);
    tui.wait_for_text("focus: stream");

    // The sidebar's `]` sends the literal Ctrl+] the stream cannot send.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("]");
    fixture.first.read_until(&[0x1d], STEP);
    assert!(tui.text().contains("focus: sidebar"), "it does not switch");
    tui.send(KEY_CTRL_BRACKET);

    // An AI client joins the same device while the human is attached.
    let ai = connect(&fixture.socket, ClientInfo::ai("pty-ai"));
    let opened = ai
        .open(&first_path, None, true)
        .expect("the AI joins the open device");
    ai.write_bytes(opened.device_id, b"from-the-ai\r\n", None)
        .expect("AI write");
    tui.wait_for_text("from-the-ai");
    tui.wait_for_text("AI: pty-ai");

    // Scrollback off: `b` explains there is nothing to show.
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send(KEY_DOWN);
    tui.send_text(" ");
    wait_flag(&mut tui, "Scrollback", "off");
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.wait_for_text("focus: stream");
    tui.send(KEY_CTRL_BRACKET);
    tui.wait_for_text("focus: sidebar");
    tui.send_text("b");
    tui.wait_for_text("scrollback is off");

    // Local echo off: typed keys reach the device but are not drawn, while
    // the device's own output is.
    tui.click(3, tui.rows() - 3);
    tui.wait_for_text("focus: settings");
    tui.send_text(" ");
    wait_flag(&mut tui, "Local echo", "off");
    tui.send(KEY_CTRL_S);
    tui.wait_for_text("settings saved");
    tui.wait_for_text("focus: stream");
    tui.send_text("WWW");
    fixture.first.read_until(b"WWW", STEP);
    // Device output after the typed keys is the render barrier: a local echo
    // would have been drawn before it, so the absence below is meaningful.
    fixture.first.write(b"device-echo\r\n");
    tui.wait_for_text("device-echo");
    assert!(
        !tui.text().contains("WWW"),
        "local echo is off:\n{}",
        tui.text()
    );
}

/// A quitting terminal leaves the alternate screen and restores cooked
/// input: `quit` fails on a non-zero exit, a screen left in alt mode, or a
/// PTY still in raw mode.
#[test]
fn ctrl_q_from_the_sidebar_quits_and_restores_the_terminal() {
    let fixture = Fixture::new();
    let mut tui = fixture.tui();
    tui.wait_for_text("Devices (");
    tui.quit();
}
