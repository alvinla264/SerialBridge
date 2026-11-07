//! The TUI's state: devices, open sessions, selection, prompt and notices.
//!
//! This module has no terminal or socket code, so the interesting behaviour
//! (switching, filtering human writes, unplug notices) is testable on its own.

use std::collections::BTreeMap;

use serial_nexus_protocol::{Event, Holder, Source, DEFAULT_BAUD};

use crate::screen::Screen;

/// Which pane receives keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Devices,
    Stream,
}

impl Focus {
    pub fn toggled(self) -> Self {
        match self {
            Self::Devices => Self::Stream,
            Self::Stream => Self::Devices,
        }
    }
}

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
    pub selected: usize,
    pub focus: Focus,
    pub sessions: BTreeMap<u64, Session>,
    pub notice: Option<Notice>,
    pub prompt: Option<BaudPrompt>,
    pub quit: bool,
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
            focus: Focus::Devices,
            sessions: BTreeMap::new(),
            notice: None,
            prompt: None,
            quit: false,
            rows: rows.max(1),
            cols: cols.max(1),
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

    pub fn selected_session_mut(&mut self) -> Option<&mut Session> {
        let id = self.selected_device_id()?;
        self.sessions.get_mut(&id)
    }

    pub fn select_delta(&mut self, delta: i32) {
        if self.devices.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.devices.len() - 1;
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, last as i32) as usize;
    }

    pub fn select_path(&mut self, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.path == path)
            .map(|index| {
                self.selected = index;
            })
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
                self.selected = index;
                return;
            }
        }
        self.selected = self.selected.min(self.devices.len().saturating_sub(1));
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
            self.selected = index;
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
