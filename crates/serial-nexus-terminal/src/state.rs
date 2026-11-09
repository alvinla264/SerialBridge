//! The TUI's state: devices, open sessions, selection, prompt and notices.
//!
//! This module has no terminal or socket code, so the interesting behaviour
//! (switching, filtering human writes, unplug notices) is testable on its own.

use std::collections::BTreeMap;

use serial_nexus_protocol::{Event, Holder, Source, DEFAULT_BAUD};

use crate::screen::Screen;
use crate::settings::TerminalSettings;

/// Which view receives keys. The sidebar is shown while choosing a device and
/// hides itself in the stream, so the device gets the whole screen. Settings is
/// its own view in the terminal area, opened from the sidebar's button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Devices,
    Stream,
    Settings,
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
pub struct AppState {
    pub devices: Vec<DeviceEntry>,
    /// The selected sidebar row: a device, or the Settings button when it is
    /// one past the last device.
    pub selected: usize,
    pub view: View,
    pub sessions: BTreeMap<u64, Session>,
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
    /// The device path the settings were opened from, put back on the way out.
    /// A path survives the device list being re-sorted or changing while the
    /// settings are open; a row number would not.
    selection_before_settings: Option<String>,
    /// The last device that was selected, so leaving the Settings button lands
    /// on a device even after discovery changed the list.
    last_device_path: Option<String>,
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
            view: View::Devices,
            sessions: BTreeMap::new(),
            notice: None,
            prompt: None,
            quit: false,
            settings_row: 0,
            settings: TerminalSettings::default(),
            draft: TerminalSettings::default(),
            selection_before_settings: None,
            last_device_path: None,
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

    /// Start editing the settings: the draft begins as a copy of what is in
    /// effect, so Escape can throw every change away.
    pub fn begin_settings(&mut self) {
        self.draft = self.settings.clone();
        self.settings_row = 0;
        // Coming from the Settings button itself, fall back to the last device
        // row, so leaving the settings lands on a device.
        // Opening the settings abandons a baud prompt: the settings keys would
        // otherwise reach the hidden prompt.
        self.prompt = None;
        self.selection_before_settings = self
            .devices
            .get(self.selected)
            .map(|device| device.path.clone())
            .or_else(|| self.last_device_path.clone());
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
        self.leave_settings();
    }

    fn leave_settings(&mut self) {
        // The Settings button is not a device: go back to the device the
        // settings were opened from, found by path in the list as it is now.
        let row = self
            .selection_before_settings
            .as_deref()
            .and_then(|path| self.devices.iter().position(|device| device.path == path))
            .unwrap_or_else(|| self.last_device_index());
        self.select_index(row);
        self.view = View::Devices;
    }

    /// Whether the sidebar's Settings button is the selected row.
    pub fn settings_button_selected(&self) -> bool {
        self.selected >= self.devices.len()
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

    /// Resize every device screen to the new stream area.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        for session in self.sessions.values_mut() {
            session.screen.resize(self.rows, self.cols);
            session.rows = self.rows;
            session.cols = self.cols;
        }
    }

    pub fn selected_device(&self) -> Option<&DeviceEntry> {
        self.devices.get(self.selected)
    }

    pub fn selected_device_id(&self) -> Option<u64> {
        self.selected_device().and_then(|device| device.device_id)
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.selected_device_id()
            .and_then(|id| self.sessions.get(&id))
    }

    /// The device the header names: the selected one, or the last device row
    /// when the Settings button is selected.
    pub fn shown_device(&self) -> Option<&DeviceEntry> {
        self.devices
            .get(self.selected)
            .or_else(|| self.devices.get(self.last_device_index()))
    }

    pub fn selected_session_mut(&mut self) -> Option<&mut Session> {
        let id = self.selected_device_id()?;
        self.sessions.get_mut(&id)
    }

    /// Select one sidebar row, keeping the last device row for when the
    /// Settings button is left. Every selection change goes through here.
    pub fn select_index(&mut self, index: usize) {
        self.selected = index.min(self.devices.len());
        if let Some(device) = self.devices.get(self.selected) {
            self.last_device_path = Some(device.path.clone());
        }
    }

    /// Move the sidebar selection over the devices and the Settings button,
    /// which is the row just past the last device.
    pub fn select_delta(&mut self, delta: i32) {
        let last = self.devices.len();
        let next = self.selected as i32 + delta;
        self.select_index(next.clamp(0, last as i32) as usize);
    }

    /// The row of the last device that was selected, in the list as it is now.
    pub fn last_device_index(&self) -> usize {
        self.last_device_path
            .as_deref()
            .and_then(|path| self.devices.iter().position(|device| device.path == path))
            .unwrap_or_else(|| self.selected.min(self.devices.len().saturating_sub(1)))
    }

    /// Point the sidebar selection at the Settings button.
    pub fn select_settings_button(&mut self) {
        self.select_index(self.devices.len());
    }

    pub fn select_path(&mut self, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.path == path)
            .map(|index| self.select_index(index))
            .is_some()
    }

    /// Replace the discovered device list, keeping the selection and any
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
        if let Some(path) = selected_path {
            if let Some(index) = self.devices.iter().position(|device| device.path == path) {
                self.select_index(index);
                return;
            }
        }
        // The Settings button stays selected when it was; otherwise a row that
        // disappeared falls back to the last device.
        self.select_index(self.selected);
    }

    /// Add a freshly opened session and point the sidebar at it. The screen
    /// starts empty and is fed only by live events, so output is never
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
            screen: Screen::new(self.rows, self.cols),
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
    }

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

    /// Start the baud prompt for the selected closed device.
    pub fn begin_baud_prompt(&mut self) -> bool {
        let Some(device) = self.selected_device() else {
            self.notice = Some(Notice::error("no device selected"));
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
    fn settings_restore_the_device_by_path_when_discovery_changes() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(1);
        state.begin_settings();

        // A device that sorts first appears while the settings are open, as an
        // AI opening it would make it do.
        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        state.cancel_settings();

        assert_eq!(
            state.selected_device().map(|device| device.path.as_str()),
            Some("/dev/b"),
            "the selected device is restored by path, not by row"
        );
    }

    #[test]
    fn settings_opened_from_the_button_return_to_the_last_device_by_path() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(0);
        state.select_settings_button();
        state.begin_settings();
        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        state.commit_settings();

        assert_eq!(
            state.selected_device().map(|device| device.path.as_str()),
            Some("/dev/a")
        );
    }

    #[test]
    fn the_last_device_index_follows_the_path_after_the_list_changes() {
        let mut state = AppState::new(10, 80);
        state.set_devices(devices(&["/dev/a", "/dev/b"]));
        state.select_index(1);
        state.set_devices(devices(&["/dev/0", "/dev/a", "/dev/b"]));
        assert_eq!(state.last_device_index(), 2);
    }
}
