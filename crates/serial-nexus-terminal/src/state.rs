//! The TUI's state: devices, open sessions, focus, prompt and notices.
//!
//! This module has no terminal or socket code, so the interesting behaviour
//! (switching, filtering human writes, unplug notices) is testable on its own.

use std::collections::BTreeMap;

use serial_nexus_protocol::{Event, Holder, Source, DEFAULT_BAUD};

use crate::screen::{Screen, SCROLLBACK_ROWS};
use crate::settings::TerminalSettings;

/// Which pane receives keys: the device sidebar on the left, or the right-hand
/// pane (the shown device's stream, its scrollback, or the settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Right,
}

/// What the right-hand pane shows. The sidebar is beside it, never instead of
/// it, except in narrow terminals where only the focused pane is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Stream,
    Settings,
    Scrollback,
}

/// The settings rows, in drawing order. Scrollback has a switch before the
/// stream can scroll: the flag is part of the settings, and the terminal keeps
/// it for the scrolling interface that follows.
pub const SETTINGS_ROWS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Error,
}

/// A message shown outside the device stream (errors, unplug, hints).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub severity: Severity,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Info,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Error,
        }
    }
}

/// One row in the sidebar: a serial device the core can see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    pub path: String,
    pub description: Option<String>,
    pub open: bool,
    pub device_id: Option<u64>,
    pub baud: Option<u32>,
    /// Short label of the AI control holder, if any.
    pub control: Option<String>,
    /// Stable core client id of the AI control holder.
    pub control_id: Option<u64>,
}

impl DeviceEntry {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            description: None,
            open: false,
            device_id: None,
            baud: None,
            control: None,
            control_id: None,
        }
    }
}

/// A device this terminal is attached to.
pub struct Session {
    pub device_id: u64,
    pub path: String,
    pub baud: u32,
    pub control: Option<String>,
    pub control_id: Option<u64>,
    pub screen: Screen,
    /// Same size as every screen; kept here to size new sessions.
    rows: usize,
    cols: usize,
}

/// Baud prompt shown when opening a closed device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaudPrompt {
    pub path: String,
    pub input: String,
    pub error: Option<String>,
}

impl BaudPrompt {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            input: String::new(),
            error: None,
        }
    }

    /// Typed rate, or [`DEFAULT_BAUD`] when the field is left empty.
    pub fn value(&self) -> Result<u32, String> {
        let text = self.input.trim();
        if text.is_empty() {
            return Ok(DEFAULT_BAUD);
        }
        match text.parse::<u32>() {
            Ok(baud) if (1..=4_000_000).contains(&baud) => Ok(baud),
            Ok(_) => Err(format!("{text} is outside 1..4000000")),
            Err(_) => Err(format!("{text} is not a baud rate")),
        }
    }
}

/// Everything the renderer needs.
///
/// The sidebar's highlighted row and the device shown in the right pane are
/// deliberately separate: moving or clicking the highlight never changes where
/// typed device input goes.
pub struct AppState {
    pub devices: Vec<DeviceEntry>,
    /// The highlighted sidebar row. It stays on a device while the Settings
    /// button has the focus, so discovery cannot turn the button into a device.
    pub selected: usize,
    /// Whether the sidebar's Settings button is highlighted.
    settings_button_selected: bool,
    /// Which pane receives keys.
    pub focus: Focus,
    /// What the right pane shows.
    pub view: View,
    pub sessions: BTreeMap<u64, Session>,
    /// The session shown in the right pane, if any.
    shown: Option<u64>,
    pub notice: Option<Notice>,
    pub prompt: Option<BaudPrompt>,
    pub quit: bool,
    /// Selected row of the settings view.
    pub settings_row: usize,
    /// The human's preferences in effect for this launch.
    pub settings: TerminalSettings,
    /// The settings being edited; only Ctrl+S copies them into `settings` and
    /// saves them. Escape throws them away.
    pub draft: TerminalSettings,
    rows: usize,
    cols: usize,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(24, 80)
    }
}

impl AppState {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            devices: Vec::new(),
            selected: 0,
            settings_button_selected: false,
            focus: Focus::Sidebar,
            view: View::Stream,
            sessions: BTreeMap::new(),
            shown: None,
            notice: None,
            prompt: None,
            quit: false,
            settings_row: 0,
            settings: TerminalSettings::default(),
            draft: TerminalSettings::default(),
            rows: rows.max(1),
            cols: cols.max(1),
        }
    }

    /// The value of one settings row: local echo first, then scrollback.
    pub fn settings_value(&self, row: usize) -> Option<bool> {
        match row {
            0 => Some(self.settings.local_echo),
            1 => Some(self.settings.scrollback),
            _ => None,
        }
    }

    /// The value of one settings row as the settings view shows it.
    pub fn draft_value(&self, row: usize) -> Option<bool> {
        match row {
            0 => Some(self.draft.local_echo),
            1 => Some(self.draft.scrollback),
            _ => None,
        }
    }

    /// Move the settings selection, staying on a real row.
    pub fn move_settings_row(&mut self, delta: i32) {
        let last = SETTINGS_ROWS - 1;
        self.settings_row = (self.settings_row as i32 + delta).clamp(0, last as i32) as usize;
    }

    /// Flip the selected setting in the draft, returning its new value.
    pub fn toggle_draft(&mut self) -> Option<bool> {
        match self.settings_row {
            0 => {
                self.draft.local_echo = !self.draft.local_echo;
                Some(self.draft.local_echo)
            }
            1 => {
                self.draft.scrollback = !self.draft.scrollback;
                Some(self.draft.scrollback)
            }
            _ => None,
        }
    }

    /// Whether the settings are open with changes that Save or Cancel would
    /// still keep or throw away.
    pub fn has_unsaved_settings(&self) -> bool {
        self.view == View::Settings && self.draft != self.settings
    }

    /// Open the settings in the right pane. The draft begins as a copy of what
    /// is in effect, so Escape can throw every change away. Re-opening the
    /// settings that are already open keeps the draft exactly as it is.
    pub fn begin_settings(&mut self) {
        // Opening the settings abandons a baud prompt: the settings keys would
        // otherwise reach the hidden prompt.
        self.prompt = None;
        self.focus = Focus::Right;
        if self.view == View::Settings {
            return;
        }
        self.draft = self.settings.clone();
        self.settings_row = 0;
        self.view = View::Settings;
        self.notice = None;
    }

    /// Leave the settings without keeping the draft.
    pub fn cancel_settings(&mut self) {
        self.draft = self.settings.clone();
        self.leave_settings();
    }

    /// Keep the draft as the settings in effect and leave the view.
    pub fn commit_settings(&mut self) {
        self.settings = self.draft.clone();
        self.apply_scrollback();
        self.leave_settings();
    }

    /// Keep every open screen's scrollback in line with the settings in effect.
    /// Turning it off frees the kept rows and brings the view back to live.
    fn apply_scrollback(&mut self) {
        let capacity = scrollback_capacity(&self.settings);
        for session in self.sessions.values_mut() {
            session.screen.set_scrollback(capacity);
        }
    }

    /// Return to the stream of the device the right pane was showing. Save and
    /// Cancel both land here, on the live screen the human was looking at.
    fn leave_settings(&mut self) {
        self.view = View::Stream;
        self.focus = Focus::Right;
        if let Some(session) = self.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
    }

    /// Whether the sidebar's Settings button is highlighted.
    pub fn settings_button_selected(&self) -> bool {
        self.settings_button_selected
    }

    /// The name of one settings panel row.
    pub fn settings_row_name(row: usize) -> Option<&'static str> {
        match row {
            0 => Some("Local echo"),
            1 => Some("Scrollback"),
            _ => None,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Resize every device screen to the new right-pane size. Each screen keeps
    /// its kept rows and scrolled position, so a resize never teleports a
    /// session's scrollback or drops another session.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        for session in self.sessions.values_mut() {
            session.screen.resize(self.rows, self.cols);
            session.rows = self.rows;
            session.cols = self.cols;
        }
    }

    /// The highlighted device row, or `None` while the Settings button is.
    pub fn selected_device(&self) -> Option<&DeviceEntry> {
        if self.settings_button_selected {
            return None;
        }
        self.devices.get(self.selected)
    }

    /// The device shown in the right pane, found by its session.
    pub fn shown_device(&self) -> Option<&DeviceEntry> {
        let id = self.shown?;
        let session = self.sessions.get(&id)?;
        self.devices
            .iter()
            .find(|device| device.device_id == Some(id) || device.path == session.path)
    }

    /// The session shown in the right pane.
    pub fn shown_session(&self) -> Option<&Session> {
        self.shown.and_then(|id| self.sessions.get(&id))
    }

    pub fn shown_session_mut(&mut self) -> Option<&mut Session> {
        let id = self.shown?;
        self.sessions.get_mut(&id)
    }

    /// The session of the highlighted device row, if it is attached.
    pub fn highlighted_session(&self) -> Option<&Session> {
        let path = &self.selected_device()?.path;
        self.sessions.values().find(|session| &session.path == path)
    }

    pub fn highlighted_session_mut(&mut self) -> Option<&mut Session> {
        let path = self.selected_device()?.path.clone();
        self.sessions
            .values_mut()
            .find(|session| session.path == path)
    }

    /// Show a session in the right pane without changing the highlight.
    pub fn show_device(&mut self, device_id: u64) -> bool {
        if !self.sessions.contains_key(&device_id) {
            return false;
        }
        self.shown = Some(device_id);
        true
    }

    /// Select one sidebar row. The shown stream is untouched: choosing a row
    /// never redirects typed device input.
    pub fn select_index(&mut self, index: usize) {
        if index >= self.devices.len() {
            self.settings_button_selected = true;
            return;
        }
        self.settings_button_selected = false;
        self.selected = index;
    }

    /// Move the sidebar selection over the devices and the Settings button,
    /// which is the row just past the last device.
    pub fn select_delta(&mut self, delta: i32) {
        let last = self.devices.len();
        let next = self.selected as i32 + delta;
        self.select_index(next.clamp(0, last as i32) as usize);
    }

    /// Point the sidebar highlight at the Settings button. The device row
    /// stays where it was, so the button is not a row that discovery can
    /// renumber into a device.
    pub fn select_settings_button(&mut self) {
        self.settings_button_selected = true;
    }

    pub fn select_path(&mut self, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.path == path)
            .map(|index| self.select_index(index))
            .is_some()
    }

    /// Replace the discovered device list, keeping the highlight and any
    /// sessions the core did not enumerate (PTYs in tests, for example).
    pub fn set_devices(&mut self, mut devices: Vec<DeviceEntry>) {
        let selected_path = self.selected_device().map(|device| device.path.clone());
        for session in self.sessions.values() {
            if devices.iter().all(|device| device.path != session.path) {
                devices.push(DeviceEntry {
                    path: session.path.clone(),
                    description: None,
                    open: true,
                    device_id: Some(session.device_id),
                    baud: Some(session.baud),
                    control: session.control.clone(),
                    control_id: session.control_id,
                });
            }
        }
        // The core's device list is authoritative for open state and the AI
        // control holder; reconcile sessions by stable client id.
        for session in self.sessions.values_mut() {
            if let Some(device) = devices
                .iter()
                .find(|device| device.device_id == Some(session.device_id))
            {
                session.control = device.control.clone();
                session.control_id = device.control_id;
                if let Some(baud) = device.baud {
                    session.baud = baud;
                }
            }
        }
        devices.sort_by(|left, right| left.path.cmp(&right.path));
        self.devices = devices;
        // The Settings button keeps the highlight; a device row keeps its
        // device by path when the list was re-sorted or grew.
        if self.settings_button_selected {
            return;
        }
        let row = selected_path
            .as_deref()
            .and_then(|path| self.devices.iter().position(|device| device.path == path))
            .unwrap_or(0);
        self.select_index(row);
    }

    /// Add a freshly opened session, point the sidebar at it and show it. The
    /// screen starts empty and is fed only by live events, so output is never
    /// duplicated by a history snapshot racing the live subscription.
    pub fn add_session(
        &mut self,
        device_id: u64,
        path: impl Into<String>,
        baud: u32,
        control: Option<Holder>,
    ) {
        let path = path.into();
        let session = Session {
            device_id,
            path: path.clone(),
            baud,
            control: control.as_ref().map(short_holder),
            control_id: control.as_ref().map(|holder| holder.client_id),
            screen: Screen::with_scrollback(
                self.rows,
                self.cols,
                scrollback_capacity(&self.settings),
            ),
            rows: self.rows,
            cols: self.cols,
        };
        self.sessions.insert(device_id, session);
        if let Some(device) = self.devices.iter_mut().find(|device| device.path == path) {
            device.open = true;
            device.device_id = Some(device_id);
            device.baud = Some(baud);
            device.control = control.as_ref().map(short_holder);
            device.control_id = control.as_ref().map(|holder| holder.client_id);
        } else {
            self.devices.push(DeviceEntry {
                path: path.clone(),
                description: None,
                open: true,
                device_id: Some(device_id),
                baud: Some(baud),
                control: control.as_ref().map(short_holder),
                control_id: control.as_ref().map(|holder| holder.client_id),
            });
            self.devices
                .sort_by(|left, right| left.path.cmp(&right.path));
        }
        if let Some(index) = self.devices.iter().position(|device| device.path == path) {
            self.select_index(index);
        }
        self.shown = Some(device_id);
    }

    /// Drop a session. A disconnect never closes the terminal and never drops
    /// the settings view: when the shown device goes away, another attached
    /// session takes the right pane, or the pane shows its empty hint.
    pub fn remove_session(&mut self, device_id: u64) -> Option<Session> {
        let session = self.sessions.remove(&device_id)?;
        for device in &mut self.devices {
            if device.device_id == Some(device_id) || device.path == session.path {
                device.open = false;
                device.device_id = None;
                device.baud = None;
                device.control = None;
                device.control_id = None;
            }
        }
        if self.shown == Some(device_id) {
            self.shown = self
                .sessions
                .values()
                .min_by(|left, right| left.path.cmp(&right.path))
                .map(|session| session.device_id);
            if self.view == View::Scrollback {
                // The scrollback of the session that went away is gone, so a
                // remaining one starts on its own live screen; its kept rows
                // and scrolled position are untouched.
                self.view = View::Stream;
            }
            if self.view == View::Stream {
                // The stream forwards keys to the device, so it always shows
                // the live screen of the session that takes over.
                if let Some(session) = self.shown.and_then(|id| self.sessions.get_mut(&id)) {
                    session.screen.scroll_to_bottom();
                }
            }
        }
        Some(session)
    }

    pub fn all_device_ids(&self) -> Vec<u64> {
        self.sessions.keys().copied().collect()
    }

    /// Apply one core event. Device output goes to its screen, AI writes are
    /// shown, human writes are never echoed, and closure becomes a notice.
    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::DeviceOutput { device_id, .. } => {
                if let Some(session) = self.sessions.get_mut(device_id) {
                    if let Some(Ok(bytes)) = event.bytes() {
                        session.screen.feed(&bytes);
                    }
                }
            }
            Event::Write {
                device_id, source, ..
            } => {
                if *source == Source::Ai {
                    if let Some(session) = self.sessions.get_mut(device_id) {
                        if let Some(Ok(bytes)) = event.bytes() {
                            session.screen.feed_ai(&bytes);
                        }
                    }
                }
            }
            Event::DeviceClosed {
                device_id,
                path,
                reason,
            } => {
                self.remove_session(*device_id);
                self.notice = Some(Notice::error(format!("unplugged: {path} ({reason})")));
            }
            Event::ControlGranted { device_id, holder } => {
                self.set_control(*device_id, Some(holder));
            }
            Event::ControlReleased {
                device_id,
                holder,
                reason,
            } => {
                // Match the stable client id: a release event may omit the
                // name the holder had when control was granted.
                let matches = self
                    .sessions
                    .get(device_id)
                    .is_some_and(|session| session.control_id == Some(holder.client_id))
                    || self.devices.iter().any(|device| {
                        device.device_id == Some(*device_id)
                            && device.control_id == Some(holder.client_id)
                    });
                if matches {
                    self.set_control(*device_id, None);
                }
                self.notice = Some(Notice::info(format!("AI control released ({reason:?})")));
            }
            Event::ControlDenied {
                device_id, holder, ..
            } => {
                self.set_control(*device_id, Some(holder));
            }
            Event::TransportClosed { reason } => {
                self.notice = Some(Notice::error(format!("core connection closed: {reason}")));
            }
            Event::CoreShutdown { reason } => {
                self.notice = Some(Notice::info(format!("core shutting down: {reason}")));
            }
            Event::ClientJoined { .. } | Event::ClientLeft { .. } => {}
        }
    }

    fn set_control(&mut self, device_id: u64, holder: Option<&Holder>) {
        let label = holder.map(short_holder);
        let control_id = holder.map(|holder| holder.client_id);
        if let Some(session) = self.sessions.get_mut(&device_id) {
            session.control = label.clone();
            session.control_id = control_id;
        }
        for device in &mut self.devices {
            if device.device_id == Some(device_id) {
                device.control = label.clone();
                device.control_id = control_id;
            }
        }
    }

    /// Start the baud prompt for the highlighted closed device.
    pub fn begin_baud_prompt(&mut self) -> bool {
        let Some(device) = self.selected_device() else {
            self.notice = Some(Notice::error("no device highlighted"));
            return false;
        };
        if device.device_id.is_some() {
            return false;
        }
        let path = device.path.clone();
        self.prompt = Some(BaudPrompt::new(path));
        true
    }

    pub fn prompt_push(&mut self, ch: char) {
        if let Some(prompt) = &mut self.prompt {
            if ch.is_ascii_digit() && prompt.input.len() < 8 {
                prompt.input.push(ch);
                prompt.error = None;
            }
        }
    }

    pub fn prompt_backspace(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.input.pop();
            prompt.error = None;
        }
    }
}

/// How many rows a screen keeps for scrolling, from the settings in effect.
fn scrollback_capacity(settings: &TerminalSettings) -> usize {
    if settings.scrollback {
        SCROLLBACK_ROWS
    } else {
        0
    }
}

pub fn short_holder(holder: &Holder) -> String {
    match &holder.name {
        Some(name) => name.clone(),
        None => format!("client {}", holder.client_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(paths: &[&str]) -> Vec<DeviceEntry> {
        paths.iter().map(|path| DeviceEntry::new(*path)).collect()
    }

    #[test]
    fn highlighting_a_row_does_not_change_the_shown_stream() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.add_session(7, "/dev/a", 115_200, None);
        state.select_index(1);

        assert_eq!(state.selected_device().unwrap().path, "/dev/b");
        assert_eq!(
            state.shown_session().map(|session| session.device_id),
            Some(7),
            "the stream still belongs to the device that was opened"
        );
    }

    #[test]
    fn a_disconnect_shows_another_attached_session_and_keeps_settings_open() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.add_session(8, "/dev/b", 115_200, None);
        state.begin_settings();
        state.draft.local_echo = false;

        state.apply_event(&Event::DeviceClosed {
            device_id: 8,
            path: "/dev/b".into(),
            reason: "removed".into(),
        });

        assert_eq!(
            state.shown_session().map(|session| session.device_id),
            Some(7),
            "the other attached session takes the right pane"
        );
        assert_eq!(state.view, View::Settings, "the settings stay open");
        assert!(!state.draft.local_echo, "the draft is kept");
        let notice = state.notice.as_ref().expect("the disconnect is reported");
        assert!(notice.text.contains("unplugged"), "{}", notice.text);
    }

    #[test]
    fn the_last_disconnect_leaves_the_stream_view_empty() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.view = View::Scrollback;

        state.remove_session(7);

        assert!(state.shown_session().is_none());
        assert_eq!(
            state.view,
            View::Stream,
            "the scrollback has nothing to show"
        );
    }

    #[test]
    fn restoring_the_highlight_by_path_survives_discovery_changes() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(1);
        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        assert_eq!(
            state.selected_device().map(|device| device.path.as_str()),
            Some("/dev/b"),
            "the highlight is restored by path, not by row"
        );
    }

    #[test]
    fn button_focus_survives_the_device_list_growing() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(0);
        state.select_settings_button();
        assert!(state.settings_button_selected());

        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        assert!(
            state.settings_button_selected(),
            "the button keeps the highlight"
        );
        assert_eq!(state.selected_device(), None);
    }

    #[test]
    fn save_and_cancel_return_to_the_shown_stream_not_the_highlight() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(1);
        state.begin_settings();
        state.draft.local_echo = false;

        state.cancel_settings();
        assert_eq!(state.view, View::Stream);
        assert_eq!(state.focus, Focus::Right);
        assert_eq!(
            state.shown_session().map(|session| session.device_id),
            Some(7),
            "the shown device did not follow the highlight"
        );
        assert_eq!(state.selected_device().unwrap().path, "/dev/b");

        state.begin_settings();
        state.draft.local_echo = false;
        state.commit_settings();
        assert_eq!(state.view, View::Stream);
        assert_eq!(state.shown_session().unwrap().device_id, 7);
    }

    #[test]
    fn reopening_the_open_settings_keeps_the_draft() {
        let mut state = AppState::new(10, 80);
        state.begin_settings();
        state.draft.local_echo = false;
        state.begin_settings();
        assert!(!state.draft.local_echo, "the unsaved draft is kept");
        assert_eq!(state.view, View::Settings);
    }

    #[test]
    fn unsaved_settings_are_only_reported_while_the_draft_differs() {
        let mut state = AppState::new(10, 80);
        assert!(!state.has_unsaved_settings());
        state.begin_settings();
        assert!(!state.has_unsaved_settings(), "a fresh draft matches");
        state.draft.local_echo = false;
        assert!(state.has_unsaved_settings());
        state.cancel_settings();
        assert!(!state.has_unsaved_settings());
    }

    fn session_state(rows: usize, scrollback: bool) -> AppState {
        let mut state = AppState::new(rows, 20);
        state.settings.scrollback = scrollback;
        state.draft = state.settings.clone();
        state.add_session(7, "/dev/ttyUSB0", 115_200, None);
        state
    }

    fn write_lines(state: &mut AppState, first: usize, count: usize) {
        for index in first..first + count {
            let line = format!("line{index}\r\n");
            state.apply_event(&Event::device_output(7, line.as_bytes()));
        }
    }

    #[test]
    fn a_session_keeps_scrolled_off_output_and_scrolls_back_through_it() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 8);
        let session = state.sessions.get(&7).expect("session");
        assert_eq!(session.screen.history_len(), 5, "five lines scrolled off");
        assert_eq!(session.screen.scroll(), 0, "the live view is the start");

        let session = state.sessions.get_mut(&7).unwrap();
        session.screen.scroll_back(4);
        assert_eq!(session.screen.scroll(), 4);
        let row: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();
        assert_eq!(row.trim_end(), "line1");

        session.screen.scroll_to_top();
        assert_eq!(session.screen.scroll(), session.screen.history_len());
        let row: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();
        assert_eq!(row.trim_end(), "line0");

        session.screen.scroll_to_bottom();
        assert_eq!(session.screen.scroll(), 0);
        assert!(!session.screen.is_scrolled_back());
    }

    #[test]
    fn scrolling_back_while_output_arrives_keeps_the_same_text_in_view() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 8);
        let session = state.sessions.get_mut(&7).unwrap();
        session.screen.scroll_back(3);
        let before: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();

        write_lines(&mut state, 8, 2);
        let session = state.sessions.get(&7).unwrap();
        assert!(session.screen.scroll() > 3, "the view stayed on its text");
        let after: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();
        assert_eq!(after, before);
    }

    #[test]
    fn without_the_scrollback_setting_nothing_is_kept_or_scrolled() {
        let mut state = session_state(4, false);
        write_lines(&mut state, 0, 12);
        let session = state.sessions.get_mut(&7).unwrap();
        assert_eq!(session.screen.history_len(), 0);
        session.screen.scroll_back(5);
        assert_eq!(session.screen.scroll(), 0, "nothing to scroll to");
    }

    #[test]
    fn saving_scrollback_off_clears_every_sessions_history() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 12);
        assert!(state.sessions.get(&7).unwrap().screen.history_len() > 0);

        state.begin_settings();
        state.draft.scrollback = false;
        state.commit_settings();
        assert!(!state.settings.scrollback);
        let session = state.sessions.get(&7).unwrap();
        assert_eq!(session.screen.history_len(), 0, "the rows are freed");
        assert_eq!(session.screen.scroll(), 0, "the view is back on live");
    }

    #[test]
    fn turning_scrollback_on_keeps_new_rows_from_then_on() {
        let mut state = session_state(4, false);
        write_lines(&mut state, 0, 8);
        state.begin_settings();
        state.draft.scrollback = true;
        state.commit_settings();
        write_lines(&mut state, 8, 8);
        assert!(state.sessions.get(&7).unwrap().screen.history_len() > 0);
    }

    #[test]
    fn resizing_keeps_every_sessions_scroll_position() {
        let mut state = session_state(6, true);
        write_lines(&mut state, 0, 12);
        state.sessions.get_mut(&7).unwrap().screen.scroll_back(4);
        let scroll = state.sessions.get(&7).unwrap().screen.scroll();
        assert!(scroll > 0);

        state.resize(4, 30);
        assert_eq!(
            state.sessions.get(&7).unwrap().screen.scroll(),
            scroll,
            "a resize keeps the scrolled position"
        );
        state.resize(8, 50);
        assert_eq!(state.sessions.get(&7).unwrap().screen.scroll(), scroll);
    }
}
