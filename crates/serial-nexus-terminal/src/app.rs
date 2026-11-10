//! The event-driven app: client calls behind small state transitions that do
//! not depend on a real terminal.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use crossterm::event::MouseEventKind;
use ratatui::layout::Rect;
use serial_nexus_client::{Client, ClientError};
use serial_nexus_protocol::{DeviceSummary, Event};

use crate::input::{echo_key, map_key, Action, Key};
use crate::render::{hit_test, panes, settings_hit_test, Hit, SettingsHit};
use crate::settings::{SettingsStore, TerminalSettings};
use crate::state::{short_holder, AppState, DeviceEntry, Focus, Notice, View, SETTINGS_ROWS};

/// Two clicks on the same cell within this long count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What the first click of a possible double-click landed on. It is kept by
/// identity, not by cell: the first click can move the sidebar's viewport, so
/// hit-testing the second click at the same cell would find another row.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClickTarget {
    Device(String),
    Settings,
}

struct LastClick {
    at: Instant,
    column: u16,
    row: u16,
    target: Option<ClickTarget>,
}

/// Wire the app to a connected core. The core is started by
/// [`Client::connect`] (the auto-start seam), not by the TUI.
pub struct App {
    pub state: AppState,
    client: Client,
    events: Receiver<Event>,
    /// Closures this terminal asked for; their `DeviceClosed` is not an unplug.
    expected_closes: HashSet<u64>,
    /// Where a changed setting is saved, when this launch has a settings file.
    settings_store: Option<SettingsStore>,
    /// The last click, for recognising a double-click on a device row.
    last_click: Option<LastClick>,
}

impl App {
    pub fn new(client: Client, rows: usize, cols: usize) -> Self {
        Self::with_settings(client, rows, cols, TerminalSettings::default())
    }

    /// Like [`App::new`], with settings loaded for this launch.
    pub fn with_settings(
        client: Client,
        rows: usize,
        cols: usize,
        settings: TerminalSettings,
    ) -> Self {
        Self::with_preferences(client, rows, cols, settings, None)
    }

    /// Like [`App::with_settings`], and a changed setting is written back
    /// through `store`. Without a store the change still applies to this launch
    /// and is not persisted.
    pub fn with_preferences(
        client: Client,
        rows: usize,
        cols: usize,
        settings: TerminalSettings,
        store: Option<SettingsStore>,
    ) -> Self {
        let events = client.subscribe();
        let mut state = AppState::new(rows, cols);
        state.settings = settings;
        state.draft = state.settings.clone();
        let mut app = Self {
            state,
            client,
            events,
            expected_closes: HashSet::new(),
            settings_store: store,
            last_click: None,
        };
        app.refresh_devices();
        app
    }

    /// The settings this launch loaded.
    pub fn settings(&self) -> &TerminalSettings {
        &self.state.settings
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Re-read the device list from the core.
    pub fn refresh_devices(&mut self) {
        match self.client.list_devices() {
            Ok(result) => {
                let entries = result.devices.into_iter().map(device_entry).collect();
                self.state.set_devices(entries);
            }
            Err(error) => self.state.notice = Some(Notice::error(error.to_string())),
        }
    }

    /// Drain live events into the state.
    pub fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let Event::DeviceClosed { device_id, .. } = &event {
                if self.expected_closes.remove(device_id) {
                    self.state.remove_session(*device_id);
                    continue;
                }
            }
            self.state.apply_event(&event);
        }
    }

    /// Handle one key. In the focused stream, keys go to the shown device and
    /// only Ctrl+] leaves; in the sidebar, the command keys act on the
    /// highlighted row and Ctrl+Q quits.
    pub fn handle_key(&mut self, key: Key) {
        // A keyboard action changes the panes under a pending click; the next
        // click must not complete a double-click on a control that moved.
        self.last_click = None;
        let action = map_key(&self.state, key);
        match action {
            Action::Quit => self.state.quit = true,
            Action::Select(delta) => self.select_row(delta),
            Action::OpenSelected => {
                if self.state.settings_button_selected() {
                    self.open_settings();
                } else {
                    self.open_selected();
                }
            }
            Action::DisconnectSelected => self.disconnect_selected(),
            Action::StartBaudPrompt => self.start_baud_prompt(),
            Action::ToggleFocus => self.toggle_focus(),
            Action::PromptChar(ch) => self.state.prompt_push(ch),
            Action::PromptBackspace => self.state.prompt_backspace(),
            Action::PromptCancel => self.state.prompt = None,
            Action::PromptSubmit => self.submit_baud_prompt(),
            Action::PromptCancelFocus => {
                self.state.prompt = None;
                self.toggle_focus();
            }
            Action::Write(bytes) => self.write_shown(&bytes, key),
            Action::Scroll(delta) => self.scroll_stream(delta),
            Action::EnterScrollback => self.enter_scrollback(),
            Action::LeaveScrollback => self.leave_scrollback(),
            Action::OpenSettings => self.open_settings(),
            Action::SettingsMove(delta) => self.state.move_settings_row(delta),
            Action::SettingsToggle => {
                self.state.toggle_draft();
            }
            Action::SettingsSave => self.save_settings(),
            Action::SettingsCancel => self.state.cancel_settings(),
            Action::Ignore => {}
        }
    }

    /// Move the keyboard focus between the sidebar and the right pane,
    /// whatever the terminal width. The shown session, the settings draft and
    /// every screen's scroll position stay exactly as they are.
    fn toggle_focus(&mut self) {
        // A right-focused settings view owns its keys (see `map_key`); the
        // sidebar can still be focused to look around, and this brings the
        // keyboard back to the draft.
        self.state.focus = match self.state.focus {
            Focus::Sidebar => Focus::Right,
            Focus::Right => Focus::Sidebar,
        };
    }

    /// Show the highlighted device's kept rows in the right pane. The sidebar's
    /// `b` gets here; a closed device has none and `b` asks for its baud rate
    /// instead, and with the scrollback setting off there is nothing to show.
    fn enter_scrollback(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        let Some(session) = self.state.highlighted_session() else {
            self.state.notice = Some(Notice::error(
                "highlight an attached device first: press Enter to open it",
            ));
            return;
        };
        let device_id = session.device_id;
        if !self.state.settings.scrollback {
            self.state.notice = Some(Notice::error("scrollback is off in the settings"));
            return;
        }
        self.state.show_device(device_id);
        if let Some(session) = self.state.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
        self.state.view = View::Scrollback;
        self.state.focus = Focus::Right;
        self.state.notice = None;
    }

    /// Leave the scrollback view for the live stream. The stream starts at the
    /// live screen, so its keys reach the device from where they land.
    fn leave_scrollback(&mut self) {
        self.show_live_stream();
    }

    /// Show the shown device's live stream, never a scrolled-back view. Every
    /// route into the stream goes through here: the stream forwards keys to the
    /// device, so what is drawn has to be where they land.
    fn show_live_stream(&mut self) {
        if let Some(session) = self.state.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
        self.state.view = View::Stream;
        self.state.focus = Focus::Right;
    }

    /// Show one attached session in the right pane and give it the keyboard.
    fn show_session(&mut self, device_id: u64) {
        // A device opened or joined by mouse or keyboard is the surface the
        // human now works on; a baud prompt for another row must not keep the
        // keys.
        self.state.prompt = None;
        self.state.show_device(device_id);
        self.show_live_stream();
    }

    /// Scroll the scrollback view: positive goes back into the kept output,
    /// negative returns towards live.
    fn scroll_stream(&mut self, delta: i32) {
        let Some(session) = self.state.shown_session_mut() else {
            return;
        };
        if delta > 0 {
            session.screen.scroll_back(delta as usize);
        } else {
            session.screen.scroll_forward(delta.unsigned_abs() as usize);
        }
    }

    /// One mouse-wheel notch in the scrollback view: three lines back or
    /// forward. The live stream has its own keys, so the wheel does nothing
    /// there.
    pub fn handle_scroll(&mut self, up: bool) {
        if self.state.view != View::Scrollback {
            return;
        }
        const NOTCH: i32 = 3;
        self.scroll_stream(if up { NOTCH } else { -NOTCH });
    }

    /// Move the sidebar highlight over the devices and the Settings button. A
    /// huge delta (Home/End) lands on the first or last row.
    fn select_row(&mut self, delta: i32) {
        if delta == i32::MIN {
            self.state.select_index(0);
        } else if delta == i32::MAX {
            self.state.select_settings_button();
        } else {
            self.state.select_delta(delta);
        }
    }

    /// Handle one click at one cell of the screen. A sidebar device click only
    /// highlights the row; it never changes the shown stream. The Settings
    /// button opens the settings, and a click in the right pane focuses it.
    pub fn handle_click(&mut self, area: Rect, column: u16, row: u16) {
        let panes = panes(area);
        let in_sidebar = column >= panes.sidebar.x
            && column < panes.sidebar.right()
            && row >= panes.sidebar.y
            && row < panes.sidebar.bottom();
        if panes.split && in_sidebar {
            self.state.focus = Focus::Sidebar;
        }
        match hit_test(&self.state, area, column, row) {
            Some(Hit::Device(index)) => {
                // A click ends a pending baud prompt for another row; opening
                // still takes an Enter or a double-click.
                self.state.prompt = None;
                self.state.focus = Focus::Sidebar;
                self.select_row_index(index);
                return;
            }
            Some(Hit::Settings) => {
                self.open_settings();
                return;
            }
            None => {}
        }
        let right = panes.right;
        let in_right =
            column >= right.x && column < right.right() && row >= right.y && row < right.bottom();
        // In a narrow terminal the single visible pane must already be the
        // right one; clicking the sidebar does not fall through to it.
        if in_right && (panes.split || self.state.focus == Focus::Right) {
            // Clicking the visible right pane takes the keys for it, so a baud
            // prompt must not keep editing a sidebar row in the background.
            self.state.prompt = None;
            self.state.focus = Focus::Right;
            if self.state.view == View::Settings {
                match settings_hit_test(&self.state, right, column, row) {
                    Some(SettingsHit::Flag(index)) => {
                        self.state.settings_row = index.min(SETTINGS_ROWS - 1);
                        self.state.toggle_draft();
                    }
                    Some(SettingsHit::Save) => self.save_settings(),
                    Some(SettingsHit::Cancel) => self.state.cancel_settings(),
                    None => {}
                }
            }
        }
    }

    /// Handle a mouse event, recognising a double-click on a device row as
    /// "open or join this device". Two clicks on the same cell within
    /// [`DOUBLE_CLICK`] count; a single click only highlights.
    pub fn handle_mouse(
        &mut self,
        area: Rect,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        at: Instant,
    ) {
        match kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let double = self.last_click.as_ref().is_some_and(|last| {
                    last.target.is_some()
                        && last.column == column
                        && last.row == row
                        && at.saturating_duration_since(last.at) < DOUBLE_CLICK
                });
                if double {
                    let target = self.last_click.take().and_then(|last| last.target);
                    self.activate_target(target);
                } else {
                    // Remember what this click landed on before it moves the
                    // sidebar's viewport; the second click of a double-click
                    // must open that same row, not whatever moved under it.
                    let target = self.click_target(area, column, row);
                    self.last_click = Some(LastClick {
                        at,
                        column,
                        row,
                        target,
                    });
                    self.handle_click(area, column, row);
                }
            }
            MouseEventKind::ScrollUp => self.handle_scroll(true),
            MouseEventKind::ScrollDown => self.handle_scroll(false),
            _ => {}
        }
    }

    /// What a click at one cell would open on a double-click.
    fn click_target(&self, area: Rect, column: u16, row: u16) -> Option<ClickTarget> {
        match hit_test(&self.state, area, column, row) {
            Some(Hit::Device(index)) => self
                .state
                .devices
                .get(index)
                .map(|device| ClickTarget::Device(device.path.clone())),
            Some(Hit::Settings) => Some(ClickTarget::Settings),
            None => None,
        }
    }

    /// A double-click on a device row opens or joins it; on the Settings button
    /// it opens the settings; anywhere else the first click already did all
    /// there is to do (the right pane took the focus).
    pub fn handle_double_click(&mut self, area: Rect, column: u16, row: u16) {
        let target = self.click_target(area, column, row);
        self.activate_target(target);
    }

    fn activate_target(&mut self, target: Option<ClickTarget>) {
        match target {
            Some(ClickTarget::Device(path)) => {
                self.state.prompt = None;
                self.state.focus = Focus::Sidebar;
                if let Some(index) = self
                    .state
                    .devices
                    .iter()
                    .position(|device| device.path == path)
                {
                    self.state.select_index(index);
                    self.open_selected();
                }
            }
            Some(ClickTarget::Settings) => self.open_settings(),
            // The first click already did all there is to do (focus a pane).
            None => {}
        }
    }

    /// Point the sidebar highlight at one of its rows (a mouse click or a
    /// test). This never changes which device is shown in the right pane.
    pub fn select_row_index(&mut self, index: usize) {
        self.state.select_index(index);
    }

    /// Open the settings in the right pane. The current settings are copied
    /// into a draft, so Escape can throw every change away; reopening settings
    /// that are already open keeps the draft.
    pub fn open_settings(&mut self) {
        self.state.begin_settings();
    }

    /// Refuse a device-opening attempt while the settings hold unsaved
    /// changes, reporting why and keeping the draft. Returns whether it did.
    fn refuse_if_unsaved_settings(&mut self) -> bool {
        if self.state.has_unsaved_settings() {
            self.state.notice = Some(Notice::error(
                "settings have unsaved changes: Save or Cancel first",
            ));
            return true;
        }
        false
    }

    /// Save the draft settings, then keep the draft and go back to the shown
    /// device's live stream.
    ///
    /// The draft becomes the settings in effect only after the file was
    /// written: a failed save, or no settings file to write, leaves the
    /// options as they were and stays in the settings so the draft is not
    /// lost. The terminal never claims a save it did not make.
    fn save_settings(&mut self) {
        let draft = self.state.draft.clone();
        let Some(store) = &self.settings_store else {
            self.state.notice = Some(Notice::error("settings not saved: no settings file"));
            return;
        };
        match store.save(&draft) {
            Ok(()) => {
                self.state.commit_settings();
                self.state.notice = Some(Notice::info("settings saved"));
            }
            Err(error) => {
                self.state.notice = Some(Notice::error(format!("settings not saved: {error}")));
            }
        }
    }

    /// Open or join the highlighted device and show it in the right pane. A
    /// closed device asks for a baud rate first; an already-open device is
    /// joined at its active rate.
    ///
    /// While the settings hold unsaved changes, opening is refused and the
    /// draft is kept: Save or Cancel is the only way out of the settings.
    pub fn open_selected(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        let Some(device) = self.state.selected_device() else {
            self.state.notice = Some(Notice::error("no device highlighted"));
            return;
        };
        let path = device.path.clone();
        if let Some(session) = self.state.highlighted_session() {
            let device_id = session.device_id;
            self.show_session(device_id);
            self.state.notice = Some(Notice::info("already attached"));
            return;
        }
        if device.open {
            match self.open_path(&path, None) {
                Ok(_) => {}
                Err(error) => self.state.notice = Some(Notice::error(error.to_string())),
            }
        } else if !self.state.begin_baud_prompt() {
            self.state.notice = Some(Notice::error("cannot open the highlighted device"));
        }
    }

    /// Open or join `path` and show its live stream. Sessions start empty and
    /// are fed by live events only, so output races never duplicate.
    pub fn open_path(&mut self, path: &str, baud: Option<u32>) -> Result<u64, ClientError> {
        let opened = self.client.open(path, baud, false)?;
        // Rejoining a device (even the same id, while another client kept it
        // open) must not leave an old close suppression behind.
        self.expected_closes.remove(&opened.device_id);
        self.state.add_session(
            opened.device_id,
            opened.port.clone(),
            opened.baud,
            opened.control.holder.clone(),
        );
        self.show_session(opened.device_id);
        self.state.notice = Some(Notice::info(format!(
            "opened {} at {} baud",
            opened.port, opened.baud
        )));
        self.refresh_devices();
        Ok(opened.device_id)
    }

    fn submit_baud_prompt(&mut self) {
        let Some(prompt) = self.state.prompt.take() else {
            return;
        };
        // A prompt can outlive a clean settings view that then gained a draft
        // (a flag click); the opening attempt is refused like any other, and
        // the draft is kept.
        if self.refuse_if_unsaved_settings() {
            return;
        }
        match prompt.value() {
            Ok(baud) => match self.open_path(&prompt.path, Some(baud)) {
                Ok(_) => {}
                Err(error) => {
                    let mut prompt = prompt;
                    prompt.error = Some(error.to_string());
                    self.state.prompt = Some(prompt);
                }
            },
            Err(message) => {
                let mut prompt = prompt;
                prompt.error = Some(message);
                self.state.prompt = Some(prompt);
            }
        }
    }

    fn start_baud_prompt(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        if self.state.begin_baud_prompt() {
            return;
        }
        let Some(device) = self.state.selected_device() else {
            self.state.notice = Some(Notice::error("no device highlighted"));
            return;
        };
        if let Some(baud) = device.baud {
            self.state.notice = Some(Notice::info(format!(
                "already open at {baud} baud; Enter joins at the active rate"
            )));
        }
    }

    /// Explicitly detach the highlighted device. Other sessions stay
    /// connected, and the shown stream only changes if that device was shown.
    pub fn disconnect_selected(&mut self) {
        let Some(session) = self.state.highlighted_session() else {
            self.state.notice = Some(Notice::error("highlight an attached device first"));
            return;
        };
        let device_id = session.device_id;
        let path = session.path.clone();
        self.expected_closes.insert(device_id);
        match self.client.close(device_id) {
            Ok(()) => {
                self.state.remove_session(device_id);
                self.state.notice = Some(Notice::info(format!("disconnected {path}")));
            }
            Err(error) => {
                if error.is_device_gone() {
                    self.state.remove_session(device_id);
                }
                self.state.notice =
                    Some(Notice::error(format!("disconnect {path} failed: {error}")));
            }
        }
        self.refresh_devices();
    }

    /// Write human input to the shown device without taking AI control. With
    /// `local_echo` on, the typed text is shown locally too; it is added only
    /// after the write was accepted, so a refused write leaves no ghost.
    ///
    /// The sidebar highlight plays no part here: typed device input goes only
    /// to the stream that has the focus and is on screen.
    pub fn write_shown(&mut self, bytes: &[u8], key: Key) {
        let Some(session) = self.state.shown_session() else {
            self.state.notice = Some(Notice::error(
                "no device shown — highlight one and press Enter to open it",
            ));
            return;
        };
        let device_id = session.device_id;
        let path = session.path.clone();
        if let Err(error) = self.client.write_bytes(device_id, bytes, None) {
            if error.is_device_gone() {
                self.state.remove_session(device_id);
            }
            self.state.notice = Some(Notice::error(format!("write to {path}: {error}")));
            return;
        }
        let local_echo = self.state.settings.local_echo;
        if local_echo {
            if let (Some(echo), Some(session)) =
                (echo_key(key), self.state.sessions.get_mut(&device_id))
            {
                session.screen.echo(&echo);
            }
        }
    }

    /// Detach every device. Called on exit and after an event-loop error.
    pub fn shutdown(&mut self) {
        let device_ids = self.state.all_device_ids();
        self.expected_closes.extend(device_ids.iter().copied());
        for device_id in device_ids {
            let _ = self.client.close(device_id);
            self.state.remove_session(device_id);
        }
        self.pump();
    }

    pub fn resize_stream(&mut self, rows: usize, cols: usize) {
        self.state.resize(rows, cols);
    }

    /// The terminal was resized: a pending first click must not complete a
    /// double-click on a control that has moved to another cell.
    pub fn handle_resize(&mut self) {
        self.last_click = None;
    }
}

fn device_entry(device: DeviceSummary) -> DeviceEntry {
    DeviceEntry {
        path: device.path,
        description: device.description,
        open: device.open,
        device_id: device.device_id,
        baud: device.baud,
        control: device.control_holder.as_ref().map(short_holder),
        control_id: device
            .control_holder
            .as_ref()
            .map(|holder| holder.client_id),
    }
}
