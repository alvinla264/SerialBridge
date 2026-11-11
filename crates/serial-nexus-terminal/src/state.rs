//! The TUI's state: devices, open sessions, focus, prompt and notices.
//!
//! This module has no terminal or socket code, so the interesting behaviour
//! (switching, filtering human writes, unplug notices) is testable on its own.

use std::collections::{BTreeMap, BTreeSet};

use serial_nexus_protocol::{
    Event, Holder, ShareSessionInfo, SharingSnapshot, Source, DEFAULT_BAUD,
};

use crate::screen::{Screen, SCROLLBACK_ROWS};
use crate::settings::TerminalSettings;

/// Which pane receives keys: the device sidebar on the left, or the right-hand
/// pane (the shown device's stream, its scrollback, or the settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Right,
}

/// Which sidebar button the highlight is on, when it is not on a device row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarButton {
    /// Start or stop the opt-in LAN sharing session.
    Share,
    /// Pair with a remote host and connect to it.
    Connect,
    /// Open the settings in the right pane.
    Settings,
}

/// What the right-hand pane shows. The sidebar is beside it, never instead of
/// it, except in narrow terminals where only the focused pane is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Stream,
    Settings,
    Scrollback,
    /// The owner's pairing information for the running sharing session. The
    /// secrets in it exist only in memory and are drawn only here.
    Pairing,
    /// The form that asks for a remote host's address, port and pairing
    /// information.
    Connect,
}

/// The settings rows before the detected-device checklist: local echo,
/// scrollback, the sharing address and the sharing port.
pub const SETTINGS_FIXED_ROWS: usize = 4;

/// What one settings row edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsRow {
    LocalEcho,
    Scrollback,
    ShareAddress,
    SharePort,
    /// The checkbox for one detected device, by its index in `devices`.
    Device(usize),
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

/// The local owner's sharing session, as the core reports it.
///
/// `pairing` holds the pairing secrets exactly once, in memory only: they are
/// never written to the settings file, a log or a device. Checking is
/// session-only, so `pending_checked` starts empty on every launch.
pub struct SharingState {
    /// Whether this terminal owns a running session.
    pub active: bool,
    /// The owner-visible snapshot (no secrets), when active.
    pub snapshot: Option<SharingSnapshot>,
    /// The session's pairing information, including both secrets.
    pub pairing: Option<ShareSessionInfo>,
    /// Device paths checked for sharing. While a session is running this
    /// mirrors the core's authoritative checked list; before it starts it is
    /// the selection the host made in the settings.
    pub pending_checked: BTreeSet<String>,
    /// The device identity each pending path was checked as. A different
    /// device at the same path no longer matches and loses its check.
    pub pending_identities: BTreeMap<String, String>,
}

impl SharingState {
    pub fn new() -> Self {
        Self {
            active: false,
            snapshot: None,
            pairing: None,
            pending_checked: BTreeSet::new(),
            pending_identities: BTreeMap::new(),
        }
    }

    /// The checked paths, from the core when a session is running.
    pub fn checked(&self) -> BTreeSet<String> {
        match &self.snapshot {
            Some(snapshot) if self.active => snapshot
                .checked
                .iter()
                .map(|device| device.path.clone())
                .collect(),
            _ => self.pending_checked.clone(),
        }
    }

    /// Drop every secret and every trace of a session.
    pub fn clear_session(&mut self) {
        self.active = false;
        self.snapshot = None;
        self.pairing = None;
    }
}

impl Default for SharingState {
    fn default() -> Self {
        Self::new()
    }
}

/// One connected host. `0` is this machine's core; every remote link gets a
/// fresh number, so two hosts may both have a device with id 1 and a path like
/// `/dev/ttyUSB0` without colliding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(pub u64);

/// This machine's core.
pub const LOCAL_ENDPOINT: EndpointId = EndpointId(0);

/// One attached device: its endpoint and the core's device id on that host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionKey {
    pub endpoint: EndpointId,
    pub device_id: u64,
}

impl SessionKey {
    pub fn local(device_id: u64) -> Self {
        Self {
            endpoint: LOCAL_ENDPOINT,
            device_id,
        }
    }
}

/// One field of the remote-connect form, in drawing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectField {
    Address,
    Port,
    Ca,
    Fingerprint,
    Credential,
}

impl ConnectField {
    pub const ALL: [Self; 5] = [
        Self::Address,
        Self::Port,
        Self::Ca,
        Self::Fingerprint,
        Self::Credential,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Address => "Host address",
            Self::Port => "Port",
            Self::Ca => "CA certificate (base64 DER)",
            Self::Fingerprint => "Host certificate SHA-256",
            Self::Credential => "Human pairing credential",
        }
    }

    fn text_mut(self, form: &mut ConnectForm) -> &mut String {
        match self {
            Self::Address => &mut form.address,
            Self::Port => &mut form.port,
            Self::Ca => &mut form.ca,
            Self::Fingerprint => &mut form.fingerprint,
            Self::Credential => &mut form.credential,
        }
    }
}

/// The remote-connect form. Every value is memory-only: nothing here is
/// written to the settings file, logged, or sent to a device.
pub struct ConnectForm {
    /// The field that takes typed text.
    pub field: ConnectField,
    pub address: String,
    pub port: String,
    pub ca: String,
    pub fingerprint: String,
    pub credential: String,
    pub error: Option<String>,
}

impl ConnectForm {
    pub fn new() -> Self {
        Self {
            field: ConnectField::Address,
            address: String::new(),
            port: String::new(),
            ca: String::new(),
            fingerprint: String::new(),
            credential: String::new(),
            error: None,
        }
    }

    /// What the form shows for one field. The credential is masked.
    pub fn value(&self, field: ConnectField) -> String {
        match field {
            ConnectField::Address => self.address.clone(),
            ConnectField::Port => self.port.clone(),
            ConnectField::Ca => self.ca.clone(),
            ConnectField::Fingerprint => self.fingerprint.clone(),
            ConnectField::Credential => {
                if self.credential.is_empty() {
                    String::new()
                } else {
                    "••••••••".to_string()
                }
            }
        }
    }

    pub fn push(&mut self, ch: char) {
        if !ch.is_ascii_graphic() {
            return;
        }
        let field = self.field.text_mut(self);
        if field.len() < 4096 {
            field.push(ch);
        }
        self.error = None;
    }

    pub fn backspace(&mut self) {
        self.field.text_mut(self).pop();
        self.error = None;
    }

    pub fn next_field(&mut self, delta: i32) {
        let index = ConnectField::ALL
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let last = ConnectField::ALL.len() - 1;
        let next = (index as i32 + delta).clamp(0, last as i32) as usize;
        self.field = ConnectField::ALL[next];
    }
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self::new()
    }
}

/// One diagonal row of the sidebar: a host group heading or a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    Group(EndpointId),
    Device(usize),
}

/// One row in the sidebar: a serial device the core or a remote host shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    /// The host this device belongs to.
    pub endpoint: EndpointId,
    pub path: String,
    pub description: Option<String>,
    /// Opaque device-node identity, when the core could capture it.
    pub identity: Option<String>,
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
        Self::on(LOCAL_ENDPOINT, path)
    }

    /// A detected entry with a synthetic identity, for tests and local-only
    /// construction. Real discovery replaces it with the core's identity.
    pub fn on(endpoint: EndpointId, path: impl Into<String>) -> Self {
        let path = path.into();
        let identity = Some(format!("entry:{path}"));
        Self {
            endpoint,
            path,
            description: None,
            identity,
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
    pub endpoint: EndpointId,
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
    pub endpoint: EndpointId,
    pub path: String,
    pub input: String,
    pub error: Option<String>,
}

impl BaudPrompt {
    pub fn new(path: impl Into<String>) -> Self {
        Self::on(LOCAL_ENDPOINT, path)
    }

    pub fn on(endpoint: EndpointId, path: impl Into<String>) -> Self {
        Self {
            endpoint,
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
    /// Which pane receives keys.
    pub focus: Focus,
    /// What the right pane shows.
    pub view: View,
    pub sessions: BTreeMap<SessionKey, Session>,
    /// The session shown in the right pane, if any.
    shown: Option<SessionKey>,
    pub notice: Option<Notice>,
    pub prompt: Option<BaudPrompt>,
    pub quit: bool,
    /// The sidebar button that has the highlight, when no device row does.
    pub selected_button: Option<SidebarButton>,
    /// Whether the sharing address or port row is taking typed text.
    pub settings_editing: bool,
    /// The port as typed, so a half-entered value survives until Save.
    pub port_text: String,
    /// Selected row of the settings view.
    pub settings_row: usize,
    /// First line of the pairing information the right pane shows.
    pub pairing_scroll: usize,
    /// The human's preferences in effect for this launch.
    pub settings: TerminalSettings,
    /// The settings being edited; only Ctrl+S copies them into `settings` and
    /// saves them. Escape throws them away.
    pub draft: TerminalSettings,
    /// The device checkboxes in the settings draft. Session-only.
    pub draft_checked: BTreeSet<String>,
    /// The identity each checked draft path was selected as.
    pub draft_identities: BTreeMap<String, String>,
    /// This terminal's sharing session (owner only).
    pub sharing: SharingState,
    /// Labels for connected remote hosts, keyed by endpoint. The local
    /// endpoint is always present in the sidebar's grouping.
    pub remote_labels: BTreeMap<EndpointId, String>,
    /// The remote-connect form while it is open.
    pub connect: Option<ConnectForm>,
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
            selected_button: None,
            focus: Focus::Sidebar,
            view: View::Stream,
            sessions: BTreeMap::new(),
            shown: None,
            notice: None,
            prompt: None,
            quit: false,
            settings_editing: false,
            port_text: String::new(),
            settings_row: 0,
            pairing_scroll: 0,
            settings: TerminalSettings::default(),
            draft: TerminalSettings::default(),
            draft_checked: BTreeSet::new(),
            draft_identities: BTreeMap::new(),
            sharing: SharingState::new(),
            remote_labels: BTreeMap::new(),
            connect: None,
            rows: rows.max(1),
            cols: cols.max(1),
        }
    }

    /// What one settings row edits. The first four rows are fixed; the rest
    /// are one checkbox per detected device.
    pub fn settings_kind(&self, row: usize) -> Option<SettingsRow> {
        match row {
            0 => Some(SettingsRow::LocalEcho),
            1 => Some(SettingsRow::Scrollback),
            2 => Some(SettingsRow::ShareAddress),
            3 => Some(SettingsRow::SharePort),
            _ => {
                // Only this host's devices can be shared: another host's
                // checklist is never ours to edit, and equal paths must not
                // stand in for a local device.
                let local = self.local_device_indices();
                let index = *local.get(row.checked_sub(SETTINGS_FIXED_ROWS)?)?;
                Some(SettingsRow::Device(index))
            }
        }
    }

    /// The device indices that belong to this machine's core, in list order.
    fn local_device_indices(&self) -> Vec<usize> {
        self.devices
            .iter()
            .enumerate()
            .filter(|(_, device)| device.endpoint == LOCAL_ENDPOINT)
            .map(|(index, _)| index)
            .collect()
    }

    /// How many settings rows there are, including one per detected local
    /// device.
    pub fn settings_row_count(&self) -> usize {
        SETTINGS_FIXED_ROWS + self.local_device_indices().len()
    }

    /// The name of a fixed settings row. Device rows are labelled by path.
    pub fn settings_row_name(row: usize) -> Option<&'static str> {
        match row {
            0 => Some("Local echo"),
            1 => Some("Scrollback"),
            2 => Some("Share address"),
            3 => Some("Share port"),
            _ => None,
        }
    }

    /// The label the settings view draws for one row.
    pub fn settings_row_label(&self, row: usize) -> String {
        if let Some(name) = Self::settings_row_name(row) {
            return name.to_string();
        }
        match self.settings_kind(row) {
            Some(SettingsRow::Device(index)) => self
                .devices
                .get(index)
                .map(|device| device.path.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The value of one settings row as the settings view shows it.
    pub fn draft_value(&self, row: usize) -> Option<String> {
        match self.settings_kind(row)? {
            SettingsRow::LocalEcho => Some(on_off(self.draft.local_echo)),
            SettingsRow::Scrollback => Some(on_off(self.draft.scrollback)),
            SettingsRow::ShareAddress => Some(self.draft.share_address.clone()),
            SettingsRow::SharePort => Some(self.port_text.clone()),
            SettingsRow::Device(index) => {
                let path = &self.devices.get(index)?.path;
                Some(if self.draft_checked.contains(path) {
                    "on".into()
                } else {
                    "off".into()
                })
            }
        }
    }

    /// Whether the checkbox of one device row is checked in the draft.
    pub fn draft_device_checked(&self, row: usize) -> Option<bool> {
        let SettingsRow::Device(index) = self.settings_kind(row)? else {
            return None;
        };
        let path = &self.devices.get(index)?.path;
        Some(self.draft_checked.contains(path))
    }

    /// Move the settings selection, staying on a real row.
    pub fn move_settings_row(&mut self, delta: i32) {
        let last = self.settings_row_count().saturating_sub(1);
        self.settings_row = (self.settings_row as i32 + delta).clamp(0, last as i32) as usize;
        self.settings_editing = false;
    }

    /// Flip the selected flag or device checkbox in the draft. Text rows are
    /// edited by typing, never toggled, and return `None`.
    pub fn toggle_draft(&mut self) -> Option<bool> {
        match self.settings_kind(self.settings_row)? {
            SettingsRow::LocalEcho => {
                self.draft.local_echo = !self.draft.local_echo;
                Some(self.draft.local_echo)
            }
            SettingsRow::Scrollback => {
                self.draft.scrollback = !self.draft.scrollback;
                Some(self.draft.scrollback)
            }
            SettingsRow::Device(index) => {
                let device = self.devices.get(index)?;
                let path = device.path.clone();
                let checked = if self.draft_checked.remove(&path) {
                    self.draft_identities.remove(&path);
                    false
                } else {
                    // Without a captured identity the core could only match a
                    // path, so a replacement could inherit this check.
                    let Some(identity) = device.identity.clone() else {
                        self.notice = Some(Notice::error(
                            "this device has no captured identity; refresh and try again",
                        ));
                        return Some(false);
                    };
                    self.draft_identities.insert(path.clone(), identity);
                    self.draft_checked.insert(path);
                    true
                };
                Some(checked)
            }
            SettingsRow::ShareAddress | SettingsRow::SharePort => None,
        }
    }

    /// Start typing into the selected address or port row.
    pub fn begin_settings_edit(&mut self) -> bool {
        match self.settings_kind(self.settings_row) {
            Some(SettingsRow::ShareAddress) => {
                // A text field starts empty, so a value is typed rather than
                // appended to whatever was there.
                self.draft.share_address.clear();
                self.settings_editing = true;
                true
            }
            Some(SettingsRow::SharePort) => {
                self.port_text.clear();
                self.settings_editing = true;
                true
            }
            _ => false,
        }
    }

    /// Add one typed character to the selected text row.
    pub fn settings_edit_push(&mut self, ch: char) {
        if !self.settings_editing || !ch.is_ascii_graphic() {
            return;
        }
        match self.settings_kind(self.settings_row) {
            Some(SettingsRow::ShareAddress) if self.draft.share_address.len() < 64 => {
                self.draft.share_address.push(ch);
            }
            Some(SettingsRow::SharePort) if self.port_text.len() < 5 => {
                self.port_text.push(ch);
            }
            _ => {}
        }
    }

    pub fn settings_edit_backspace(&mut self) {
        if !self.settings_editing {
            return;
        }
        match self.settings_kind(self.settings_row) {
            Some(SettingsRow::ShareAddress) => {
                self.draft.share_address.pop();
            }
            Some(SettingsRow::SharePort) => {
                self.port_text.pop();
            }
            _ => {}
        }
    }

    /// Stop editing the text row. The draft keeps what was typed.
    pub fn end_settings_edit(&mut self) {
        self.settings_editing = false;
    }

    /// Whether the settings are open with changes that Save or Cancel would
    /// still keep or throw away.
    pub fn has_unsaved_settings(&self) -> bool {
        self.view == View::Settings
            && (self.draft != self.settings
                || self.draft_checked != self.sharing.checked()
                || self.port_text != self.draft.share_port.to_string())
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
        // The checklist starts from what the core currently shares, or from
        // the session-only selection made before sharing started.
        self.draft_checked = self.sharing.checked();
        self.draft_identities = self.sharing.pending_identities.clone();
        self.port_text = self.draft.share_port.to_string();
        self.settings_row = 0;
        self.settings_editing = false;
        self.view = View::Settings;
        self.notice = None;
    }

    /// Leave the settings without keeping the draft.
    pub fn cancel_settings(&mut self) {
        self.draft = self.settings.clone();
        self.draft_checked = self.sharing.checked();
        self.draft_identities = self.sharing.pending_identities.clone();
        self.port_text = self.draft.share_port.to_string();
        self.settings_editing = false;
        self.leave_settings();
    }

    /// Keep the draft (and its device checks) as the settings in effect and
    /// leave the view. The app calls this only after the file was written and
    /// the core accepted the checklist changes.
    pub fn commit_settings(&mut self) {
        self.settings = self.draft.clone();
        self.port_text = self.settings.share_port.to_string();
        self.sharing.pending_checked = self.draft_checked.clone();
        self.sharing.pending_identities = self.draft_identities.clone();
        self.settings_editing = false;
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
        // A baud prompt belongs to the sidebar; leaving the settings must not
        // hand the returned stream's keys back to it.
        self.prompt = None;
        self.view = View::Stream;
        self.focus = Focus::Right;
        if let Some(session) = self.shown_session_mut() {
            session.screen.scroll_to_bottom();
        }
    }

    /// The sidebar's rows. With a connected remote host, devices are grouped
    /// under a heading per endpoint, so identical paths on two hosts cannot be
    /// mistaken for one device.
    pub fn sidebar_rows(&self) -> Vec<SidebarRow> {
        let grouped = !self.remote_labels.is_empty();
        let mut rows = Vec::new();
        let mut last: Option<EndpointId> = None;
        for (index, device) in self.devices.iter().enumerate() {
            if grouped && last != Some(device.endpoint) {
                rows.push(SidebarRow::Group(device.endpoint));
                last = Some(device.endpoint);
            }
            rows.push(SidebarRow::Device(index));
        }
        rows
    }

    /// The heading of one endpoint's device group.
    pub fn endpoint_label(&self, endpoint: EndpointId) -> String {
        if endpoint == LOCAL_ENDPOINT {
            "Local".to_string()
        } else {
            self.remote_labels
                .get(&endpoint)
                .cloned()
                .unwrap_or_else(|| format!("Remote {}", endpoint.0))
        }
    }

    /// A short stable discriminator for one endpoint, used beside device rows
    /// and in the header when paths alone cannot tell two hosts apart.
    pub fn endpoint_tag(&self, endpoint: EndpointId) -> String {
        if endpoint == LOCAL_ENDPOINT {
            "local".to_string()
        } else {
            format!("#{}", endpoint.0)
        }
    }

    /// Whether the sidebar's Settings button is highlighted.
    pub fn settings_button_selected(&self) -> bool {
        self.selected_button == Some(SidebarButton::Settings)
    }

    /// The sidebar button with the highlight, if any.
    pub fn selected_button(&self) -> Option<SidebarButton> {
        self.selected_button
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

    /// The highlighted device row, or `None` while a sidebar button is.
    pub fn selected_device(&self) -> Option<&DeviceEntry> {
        if self.selected_button.is_some() {
            return None;
        }
        self.devices.get(self.selected)
    }

    /// The key of the session shown in the right pane, if any.
    pub fn shown_key(&self) -> Option<SessionKey> {
        self.shown
    }

    /// The device shown in the right pane, found by its session.
    pub fn shown_device(&self) -> Option<&DeviceEntry> {
        let key = self.shown?;
        let session = self.sessions.get(&key)?;
        self.devices.iter().find(|device| {
            device.endpoint == key.endpoint
                && (device.device_id == Some(key.device_id) || device.path == session.path)
        })
    }

    /// The session shown in the right pane.
    pub fn shown_session(&self) -> Option<&Session> {
        self.shown.and_then(|key| self.sessions.get(&key))
    }

    pub fn shown_session_mut(&mut self) -> Option<&mut Session> {
        let key = self.shown?;
        self.sessions.get_mut(&key)
    }

    /// The session of the highlighted device row, if it is attached.
    pub fn highlighted_session(&self) -> Option<&Session> {
        let device = self.selected_device()?;
        self.sessions
            .values()
            .find(|session| session.endpoint == device.endpoint && session.path == device.path)
    }

    /// Show a session in the right pane without changing the highlight.
    pub fn show_device(&mut self, key: SessionKey) -> bool {
        if !self.sessions.contains_key(&key) {
            return false;
        }
        self.shown = Some(key);
        true
    }

    /// Show one local session, for callers that only use this core.
    pub fn show_local_device(&mut self, device_id: u64) -> bool {
        self.show_device(SessionKey::local(device_id))
    }

    /// Select one sidebar row. The shown stream is untouched: choosing a row
    /// never redirects typed device input.
    pub fn select_index(&mut self, index: usize) {
        if index >= self.devices.len() {
            // The buttons follow the devices, in drawing order.
            self.selected_button = match index - self.devices.len() {
                0 => Some(SidebarButton::Share),
                1 => Some(SidebarButton::Connect),
                _ => Some(SidebarButton::Settings),
            };
            return;
        }
        self.selected_button = None;
        self.selected = index;
    }

    /// Move the sidebar selection over the devices and the three buttons:
    /// Share, Connect and Settings, in that drawing order.
    pub fn select_delta(&mut self, delta: i32) {
        let buttons = 3;
        let last = self.devices.len() + buttons - 1;
        let current = match self.selected_button {
            Some(SidebarButton::Share) => self.devices.len(),
            Some(SidebarButton::Connect) => self.devices.len() + 1,
            Some(SidebarButton::Settings) => self.devices.len() + 2,
            None => self.selected,
        };
        let next = current as i32 + delta;
        self.select_index(next.clamp(0, last as i32) as usize);
    }

    /// Point the sidebar highlight at the Settings button. The device row
    /// stays where it was, so the button is not a row that discovery can
    /// renumber into a device.
    pub fn select_settings_button(&mut self) {
        self.selected_button = Some(SidebarButton::Settings);
    }

    pub fn select_path(&mut self, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.path == path)
            .map(|index| self.select_index(index))
            .is_some()
    }

    pub fn select_endpoint_path(&mut self, endpoint: EndpointId, path: &str) -> bool {
        self.devices
            .iter()
            .position(|device| device.endpoint == endpoint && device.path == path)
            .map(|index| self.select_index(index))
            .is_some()
    }

    /// Replace this machine's discovered device list.
    pub fn set_devices(&mut self, devices: Vec<DeviceEntry>) {
        self.set_endpoint_devices(LOCAL_ENDPOINT, devices);
    }

    /// Replace one endpoint's discovered device list, keeping the highlight
    /// and any sessions that host did not enumerate. Other endpoints' rows are
    /// untouched, so identical paths on two hosts stay separate.
    pub fn set_endpoint_devices(&mut self, endpoint: EndpointId, mut devices: Vec<DeviceEntry>) {
        let selected = self
            .selected_device()
            .map(|device| (device.endpoint, device.path.clone()));
        for device in &mut devices {
            device.endpoint = endpoint;
        }
        // A remote host's list is authoritative: a session whose device it no
        // longer lists was revoked or unplugged, so drop it rather than
        // resurrect the row from the local session cache.
        if endpoint != LOCAL_ENDPOINT {
            let missing: Vec<(SessionKey, String)> = self
                .sessions
                .iter()
                .filter(|(_, session)| {
                    session.endpoint == endpoint
                        && devices
                            .iter()
                            .all(|device| device.device_id != Some(session.device_id))
                })
                .map(|(key, session)| (*key, session.path.clone()))
                .collect();
            if !missing.is_empty() {
                let paths: Vec<String> = missing.iter().map(|(_, path)| path.clone()).collect();
                for (key, _) in missing {
                    self.remove_session(key);
                }
                self.notice = Some(Notice::error(format!(
                    "no longer shared by the remote host: {}",
                    paths.join(", ")
                )));
            }
        }
        for session in self.sessions.values() {
            if session.endpoint == endpoint
                && devices.iter().all(|device| device.path != session.path)
            {
                devices.push(DeviceEntry {
                    endpoint,
                    path: session.path.clone(),
                    description: None,
                    identity: None,
                    open: true,
                    device_id: Some(session.device_id),
                    baud: Some(session.baud),
                    control: session.control.clone(),
                    control_id: session.control_id,
                });
            }
        }
        // The host's device list is authoritative for open state and the AI
        // control holder; reconcile this endpoint's sessions by stable id.
        for session in self
            .sessions
            .values_mut()
            .filter(|session| session.endpoint == endpoint)
        {
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
        self.devices.retain(|device| device.endpoint != endpoint);
        self.devices.extend(devices);
        self.devices.sort_by(|left, right| {
            left.endpoint
                .cmp(&right.endpoint)
                .then_with(|| left.path.cmp(&right.path))
        });
        // A button keeps the highlight; a device row keeps its device by
        // endpoint and path when the list was re-sorted or grew.
        if self.selected_button.is_some() {
            return;
        }
        let row = selected
            .and_then(|(endpoint, path)| {
                self.devices
                    .iter()
                    .position(|device| device.endpoint == endpoint && device.path == path)
            })
            .unwrap_or(0);
        self.select_index(row);
    }

    /// Add a freshly opened session, point the sidebar at it and show it. The
    /// screen starts empty and is fed only by live events, so output is never
    /// duplicated by a history snapshot racing the live subscription.
    /// Add a freshly opened local session (tests and the local-only path).
    pub fn add_session(
        &mut self,
        device_id: u64,
        path: impl Into<String>,
        baud: u32,
        control: Option<Holder>,
    ) {
        self.add_endpoint_session(LOCAL_ENDPOINT, device_id, path, baud, control);
    }

    /// Add a freshly opened session on one host, point the sidebar at it and
    /// show it. The screen starts empty and is fed only by live events.
    pub fn add_endpoint_session(
        &mut self,
        endpoint: EndpointId,
        device_id: u64,
        path: impl Into<String>,
        baud: u32,
        control: Option<Holder>,
    ) {
        let path = path.into();
        let key = SessionKey {
            endpoint,
            device_id,
        };
        let session = Session {
            endpoint,
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
        self.sessions.insert(key, session);
        if let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.endpoint == endpoint && device.path == path)
        {
            device.open = true;
            device.device_id = Some(device_id);
            device.baud = Some(baud);
            device.control = control.as_ref().map(short_holder);
            device.control_id = control.as_ref().map(|holder| holder.client_id);
        } else {
            self.devices.push(DeviceEntry {
                endpoint,
                path: path.clone(),
                description: None,
                identity: None,
                open: true,
                device_id: Some(device_id),
                baud: Some(baud),
                control: control.as_ref().map(short_holder),
                control_id: control.as_ref().map(|holder| holder.client_id),
            });
            self.devices
                .sort_by(|left, right| left.path.cmp(&right.path));
        }
        if let Some(index) = self
            .devices
            .iter()
            .position(|device| device.endpoint == endpoint && device.path == path)
        {
            self.select_index(index);
        }
        self.shown = Some(key);
    }

    /// Drop a session. A disconnect never closes the terminal and never drops
    /// the settings view: when the shown device goes away, another attached
    /// session takes the right pane, or the pane shows its empty hint.
    pub fn remove_session(&mut self, key: SessionKey) -> Option<Session> {
        let session = self.sessions.remove(&key)?;
        for device in &mut self.devices {
            if device.endpoint == key.endpoint
                && (device.device_id == Some(key.device_id) || device.path == session.path)
            {
                device.open = false;
                device.device_id = None;
                device.baud = None;
                device.control = None;
                device.control_id = None;
            }
        }
        if self.shown == Some(key) {
            self.shown = self
                .sessions
                .iter()
                .min_by(|(left_key, _), (right_key, _)| {
                    left_key.endpoint.cmp(&right_key.endpoint).then_with(|| {
                        self.sessions[left_key]
                            .path
                            .cmp(&self.sessions[right_key].path)
                    })
                })
                .map(|(key, _)| *key);
            match self.view {
                // The stream forwards keys to the device, so it always shows
                // the live screen of the session that takes over.
                View::Stream => {
                    if let Some(session) = self.shown.and_then(|key| self.sessions.get_mut(&key)) {
                        session.screen.scroll_to_bottom();
                    }
                    // A stream that suddenly belongs to another device must
                    // not swallow the next queued keys as writes: the human
                    // opens or joins it again before typing.
                    self.focus = Focus::Sidebar;
                }
                // Scrollback keys only navigate; a remaining session keeps its
                // own kept rows and scrolled position. With nothing left, the
                // view falls back to the empty stream.
                View::Scrollback if self.shown.is_none() => self.view = View::Stream,
                View::Scrollback | View::Settings | View::Pairing | View::Connect => {}
            }
        }
        Some(session)
    }

    /// Drop every session of one endpoint (its host disconnected).
    pub fn remove_endpoint_sessions(&mut self, endpoint: EndpointId) -> Vec<SessionKey> {
        let keys: Vec<SessionKey> = self
            .sessions
            .keys()
            .filter(|key| key.endpoint == endpoint)
            .copied()
            .collect();
        for key in &keys {
            self.remove_session(*key);
        }
        keys
    }

    /// Every attached session, with its endpoint and that host's device id.
    pub fn all_sessions(&self) -> Vec<SessionKey> {
        self.sessions.keys().copied().collect()
    }

    /// Take the core's authoritative sharing snapshot. A device the core no
    /// longer lists (unplugged, replaced at the same path, or revoked) also
    /// loses its checkbox: sharing it again needs an explicit check.
    pub fn apply_sharing_snapshot(&mut self, snapshot: SharingSnapshot) {
        let previous = self.sharing.checked();
        let next: BTreeSet<String> = snapshot
            .checked
            .iter()
            .map(|device| device.path.clone())
            .collect();
        for path in previous.difference(&next) {
            self.draft_checked.remove(path);
            self.draft_identities.remove(path);
        }
        self.sharing.active = snapshot.active;
        self.sharing.pending_checked = next;
        self.sharing.pending_identities = snapshot
            .checked
            .iter()
            .map(|device| (device.path.clone(), device.identity.clone()))
            .collect();
        self.sharing.snapshot = snapshot.active.then_some(snapshot);
        if !self.sharing.active {
            self.sharing.pairing = None;
            self.close_pairing_view();
        }
    }

    /// Leave a pairing view after sharing ended without the human asking.
    /// The keys go back to the sidebar, so a later `s` cannot be written to a
    /// remote stream that happens to be in the right pane.
    pub fn close_pairing_view(&mut self) {
        if self.view == View::Pairing {
            self.view = View::Stream;
            self.focus = Focus::Sidebar;
        }
    }

    /// Drop checked devices that are no longer detected. The core revokes
    /// their authorization; the checkbox must not survive a replacement.
    pub fn prune_share_checks(&mut self, detected: &BTreeMap<String, String>) {
        retain_checked(
            &mut self.sharing.pending_checked,
            &mut self.sharing.pending_identities,
            detected,
        );
        retain_checked(
            &mut self.draft_checked,
            &mut self.draft_identities,
            detected,
        );
    }

    /// Drop a check whose recorded identity no longer matches the device at
    /// that path. Called before granting or starting, so a replacement made
    /// between two refreshes is not authorized by stale UI state.
    pub fn drop_replaced_checks(&mut self) {
        let detected = self.detected_local_identities();
        self.prune_share_checks(&detected);
    }

    /// The local devices the core currently detects, mapped to their identity.
    pub fn detected_local_identities(&self) -> BTreeMap<String, String> {
        self.devices
            .iter()
            .filter(|device| device.endpoint == LOCAL_ENDPOINT)
            .filter_map(|device| {
                device
                    .identity
                    .as_ref()
                    .map(|identity| (device.path.clone(), identity.clone()))
            })
            .collect()
    }

    /// Apply one local core event.
    pub fn apply_event(&mut self, event: &Event) {
        self.apply_endpoint_event(LOCAL_ENDPOINT, event);
    }

    /// Apply one event from a specific host. Session lookups are keyed by the
    /// endpoint, so two hosts with the same device id cannot cross streams.
    pub fn apply_endpoint_event(&mut self, endpoint: EndpointId, event: &Event) {
        match event {
            Event::DeviceOutput { device_id, .. } => {
                let key = SessionKey {
                    endpoint,
                    device_id: *device_id,
                };
                if let Some(session) = self.sessions.get_mut(&key) {
                    if let Some(Ok(bytes)) = event.bytes() {
                        session.screen.feed(&bytes);
                    }
                }
            }
            Event::Write {
                device_id, source, ..
            } => {
                if *source == Source::Ai {
                    let key = SessionKey {
                        endpoint,
                        device_id: *device_id,
                    };
                    if let Some(session) = self.sessions.get_mut(&key) {
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
                let key = SessionKey {
                    endpoint,
                    device_id: *device_id,
                };
                self.remove_session(key);
                self.notice = Some(Notice::error(format!("unplugged: {path} ({reason})")));
            }
            Event::ControlGranted { device_id, holder } => {
                self.set_control(endpoint, *device_id, Some(holder));
            }
            Event::ControlReleased {
                device_id,
                holder,
                reason,
            } => {
                // Match the stable client id: a release event may omit the
                // name the holder had when control was granted.
                let key = SessionKey {
                    endpoint,
                    device_id: *device_id,
                };
                let matches = self
                    .sessions
                    .get(&key)
                    .is_some_and(|session| session.control_id == Some(holder.client_id))
                    || self.devices.iter().any(|device| {
                        device.endpoint == endpoint
                            && device.device_id == Some(*device_id)
                            && device.control_id == Some(holder.client_id)
                    });
                if matches {
                    self.set_control(endpoint, *device_id, None);
                }
                self.notice = Some(Notice::info(format!("AI control released ({reason:?})")));
            }
            Event::ControlDenied {
                device_id, holder, ..
            } => {
                self.set_control(endpoint, *device_id, Some(holder));
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

    fn set_control(&mut self, endpoint: EndpointId, device_id: u64, holder: Option<&Holder>) {
        let label = holder.map(short_holder);
        let control_id = holder.map(|holder| holder.client_id);
        let key = SessionKey {
            endpoint,
            device_id,
        };
        if let Some(session) = self.sessions.get_mut(&key) {
            session.control = label.clone();
            session.control_id = control_id;
        }
        for device in &mut self.devices {
            if device.endpoint == endpoint && device.device_id == Some(device_id) {
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
        let endpoint = device.endpoint;
        let path = device.path.clone();
        self.prompt = Some(BaudPrompt::on(endpoint, path));
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

/// Drop checks whose path is no longer detected or whose recorded identity no
/// longer matches the device at that path. A replacement at a reused path must
/// be checked again instead of inheriting the old selection.
fn retain_checked(
    checked: &mut BTreeSet<String>,
    identities: &mut BTreeMap<String, String>,
    detected: &BTreeMap<String, String>,
) {
    checked.retain(|path| match (identities.get(path), detected.get(path)) {
        (Some(recorded), Some(current)) => recorded == current,
        (Some(_), None) | (None, Some(_)) | (None, None) => false,
    });
    identities.retain(|path, _| checked.contains(path));
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
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
    fn a_disconnect_in_the_stream_returns_the_keys_to_the_sidebar() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.add_session(8, "/dev/b", 115_200, None);
        state.show_local_device(8);
        state.view = View::Stream;
        state.focus = Focus::Right;

        state.remove_session(SessionKey::local(8));

        assert_eq!(state.shown_session().unwrap().device_id, 7);
        assert_eq!(
            state.focus,
            Focus::Sidebar,
            "the next keys are commands, not writes to the fallback device"
        );
    }

    #[test]
    fn a_disconnect_in_scrollback_leaves_navigation_on_the_remaining_session() {
        let mut state = AppState::new(6, 20);
        state.add_session(7, "/dev/a", 115_200, None);
        state.add_session(8, "/dev/b", 115_200, None);
        for index in 0..12 {
            let line = format!("line{index}\r\n");
            state.apply_event(&Event::device_output(7, line.as_bytes()));
        }
        // B is the shown session; its own screen stays where it was.
        state.show_local_device(8);
        state
            .sessions
            .get_mut(&SessionKey::local(8))
            .unwrap()
            .screen
            .scroll_back(2);
        state.view = View::Scrollback;
        state.focus = Focus::Right;

        state.apply_event(&Event::DeviceClosed {
            device_id: 8,
            path: "/dev/b".into(),
            reason: "removed".into(),
        });

        assert_eq!(
            state.view,
            View::Scrollback,
            "the arrow keys still navigate, not write to the fallback"
        );
        assert_eq!(state.shown_session().unwrap().device_id, 7);
    }

    #[test]
    fn the_last_disconnect_leaves_the_stream_view_empty() {
        let mut state = AppState::new(10, 80);
        state.add_session(7, "/dev/a", 115_200, None);
        state.view = View::Scrollback;

        state.remove_session(SessionKey::local(7));

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
        let session = state.sessions.get(&SessionKey::local(7)).expect("session");
        assert_eq!(session.screen.history_len(), 5, "five lines scrolled off");
        assert_eq!(session.screen.scroll(), 0, "the live view is the start");

        let session = state.sessions.get_mut(&SessionKey::local(7)).unwrap();
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
        let session = state.sessions.get_mut(&SessionKey::local(7)).unwrap();
        session.screen.scroll_back(3);
        let before: String = (0..20)
            .map(|col| session.screen.view_cell(0, col).ch)
            .collect();

        write_lines(&mut state, 8, 2);
        let session = state.sessions.get(&SessionKey::local(7)).unwrap();
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
        let session = state.sessions.get_mut(&SessionKey::local(7)).unwrap();
        assert_eq!(session.screen.history_len(), 0);
        session.screen.scroll_back(5);
        assert_eq!(session.screen.scroll(), 0, "nothing to scroll to");
    }

    #[test]
    fn saving_scrollback_off_clears_every_sessions_history() {
        let mut state = session_state(4, true);
        write_lines(&mut state, 0, 12);
        assert!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .history_len()
                > 0
        );

        state.begin_settings();
        state.draft.scrollback = false;
        state.commit_settings();
        assert!(!state.settings.scrollback);
        let session = state.sessions.get(&SessionKey::local(7)).unwrap();
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
        assert!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .history_len()
                > 0
        );
    }

    #[test]
    fn resizing_keeps_every_sessions_scroll_position() {
        let mut state = session_state(6, true);
        write_lines(&mut state, 0, 12);
        state
            .sessions
            .get_mut(&SessionKey::local(7))
            .unwrap()
            .screen
            .scroll_back(4);
        let scroll = state
            .sessions
            .get(&SessionKey::local(7))
            .unwrap()
            .screen
            .scroll();
        assert!(scroll > 0);

        state.resize(4, 30);
        assert_eq!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .scroll(),
            scroll,
            "a resize keeps the scrolled position"
        );
        state.resize(8, 50);
        assert_eq!(
            state
                .sessions
                .get(&SessionKey::local(7))
                .unwrap()
                .screen
                .scroll(),
            scroll
        );
    }
}

#[cfg(test)]
mod sharing_tests {
    use super::*;
    use serial_nexus_protocol::{SharedDevice, SharingSnapshot};

    fn state_with_devices() -> AppState {
        let mut state = AppState::new(10, 80);
        state.set_devices(vec![
            DeviceEntry::new("/dev/ttyUSB0"),
            DeviceEntry::new("/dev/ttyUSB1"),
        ]);
        state
    }

    fn snapshot(checked: &[&str]) -> SharingSnapshot {
        SharingSnapshot {
            active: true,
            session_id: Some("session".into()),
            bind: Some("127.0.0.1:7331".parse().unwrap()),
            checked: checked
                .iter()
                .map(|path| SharedDevice {
                    path: (*path).into(),
                    identity: "dev=1:2 ino=3".into(),
                })
                .collect(),
            remote_clients: 0,
            pending_connections: 0,
        }
    }

    #[test]
    fn the_sidebar_moves_over_devices_then_share_connect_and_settings() {
        let mut state = state_with_devices();
        state.select_index(1);
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Share));
        assert!(
            state.selected_device().is_none(),
            "a button holds the highlight"
        );
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Connect));
        state.select_delta(1);
        assert_eq!(state.selected_button(), Some(SidebarButton::Settings));
        state.select_delta(1);
        assert_eq!(
            state.selected_button(),
            Some(SidebarButton::Settings),
            "the last row is the last button"
        );
        state.select_delta(-10);
        assert_eq!(state.selected_button(), None);
        assert_eq!(state.selected_device().unwrap().path, "/dev/ttyUSB0");
    }

    #[test]
    fn the_checklist_is_session_only_and_a_snapshot_revokes_a_replaced_path() {
        let mut state = state_with_devices();
        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        assert!(state.has_unsaved_settings());
        state.cancel_settings();
        assert!(state.draft_checked.is_empty());

        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        state.commit_settings();
        assert!(state.sharing.pending_checked.contains("/dev/ttyUSB0"));

        // A running session's snapshot is authoritative: a path it no longer
        // lists loses its checkbox, and the draft follows.
        state.sharing.active = true;
        state.sharing.snapshot = Some(snapshot(&["/dev/ttyUSB0"]));
        state.begin_settings();
        assert_eq!(state.draft_device_checked(SETTINGS_FIXED_ROWS), Some(true));
        state.apply_sharing_snapshot(snapshot(&[]));
        assert_eq!(
            state.draft_device_checked(SETTINGS_FIXED_ROWS),
            Some(false),
            "a replacement at the path needs an explicit recheck"
        );
        assert!(state.sharing.pending_checked.is_empty());
    }

    #[test]
    fn the_host_checklist_lists_only_this_machines_devices() {
        let mut state = AppState::new(10, 80);
        let mut remote = DeviceEntry::on(EndpointId(7), "/dev/ttyREMOTE0");
        remote.identity = Some("remote-id".into());
        state.set_endpoint_devices(LOCAL_ENDPOINT, vec![DeviceEntry::new("/dev/ttyLOCAL0")]);
        state.set_endpoint_devices(EndpointId(7), vec![remote]);
        assert_eq!(state.settings_row_count(), SETTINGS_FIXED_ROWS + 1);
        assert_eq!(
            state.settings_kind(SETTINGS_FIXED_ROWS),
            Some(SettingsRow::Device(0))
        );
        assert_eq!(state.settings_kind(SETTINGS_FIXED_ROWS + 1), None);
        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        assert!(state.draft_checked.contains("/dev/ttyLOCAL0"));
        assert!(!state.draft_checked.contains("/dev/ttyREMOTE0"));
    }

    #[test]
    fn a_replacement_identity_loses_its_check_before_sharing_starts() {
        let mut state = state_with_devices();
        let mut device = DeviceEntry::new("/dev/ttyUSB0");
        device.identity = Some("old-node".into());
        state.set_devices(vec![device, DeviceEntry::new("/dev/ttyUSB1")]);
        state.begin_settings();
        state.settings_row = SETTINGS_FIXED_ROWS;
        assert_eq!(state.toggle_draft(), Some(true));
        assert!(state.draft_checked.contains("/dev/ttyUSB0"));

        let detected: BTreeMap<String, String> = [
            ("/dev/ttyUSB0".to_string(), "new-node".to_string()),
            ("/dev/ttyUSB1".to_string(), "other-node".to_string()),
        ]
        .into_iter()
        .collect();
        state.prune_share_checks(&detected);
        assert!(
            !state.draft_checked.contains("/dev/ttyUSB0"),
            "a different node at the reused path needs a new check"
        );
    }

    #[test]
    fn an_unplugged_path_is_pruned_from_the_checks() {
        let mut state = state_with_devices();
        state.sharing.pending_checked.insert("/dev/ttyUSB0".into());
        state
            .sharing
            .pending_identities
            .insert("/dev/ttyUSB0".into(), "gone".into());
        state.draft_checked.insert("/dev/ttyUSB1".into());
        state
            .draft_identities
            .insert("/dev/ttyUSB1".into(), "id1".into());
        let detected: BTreeMap<String, String> = [("/dev/ttyUSB1".to_string(), "id1".to_string())]
            .into_iter()
            .collect();
        state.prune_share_checks(&detected);
        assert!(!state.sharing.pending_checked.contains("/dev/ttyUSB0"));
        assert!(state.draft_checked.contains("/dev/ttyUSB1"));
    }

    #[test]
    fn text_rows_are_edited_by_typing_not_toggled() {
        let mut state = state_with_devices();
        state.begin_settings();
        state.settings_row = 2;
        assert_eq!(state.toggle_draft(), None, "the address is not a flag");
        assert!(state.begin_settings_edit());
        state.settings_edit_push('1');
        state.settings_edit_push('.');
        assert_eq!(state.draft.share_address, "1.");
        state.settings_edit_backspace();
        assert_eq!(state.draft.share_address, "1");
        state.end_settings_edit();

        state.settings_row = 3;
        assert_eq!(state.toggle_draft(), None);
        assert!(state.begin_settings_edit());
        state.settings_edit_push('9');
        assert_eq!(state.port_text, "9", "the port is typed as text");
        state.cancel_settings();
        assert_eq!(state.port_text, "7331");
    }

    #[test]
    fn settings_row_kinds_follow_the_detected_devices() {
        let state = state_with_devices();
        assert_eq!(state.settings_row_count(), SETTINGS_FIXED_ROWS + 2);
        assert_eq!(state.settings_kind(0), Some(SettingsRow::LocalEcho));
        assert_eq!(state.settings_kind(2), Some(SettingsRow::ShareAddress));
        assert_eq!(state.settings_kind(3), Some(SettingsRow::SharePort));
        assert_eq!(state.settings_kind(4), Some(SettingsRow::Device(0)));
        assert_eq!(state.settings_kind(5), Some(SettingsRow::Device(1)));
        assert_eq!(state.settings_kind(6), None);
    }
}
