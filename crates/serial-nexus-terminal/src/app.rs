//! The event-driven app: client calls behind small state transitions that do
//! not depend on a real terminal.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use crossterm::event::MouseEventKind;
use ratatui::layout::Rect;
use serial_nexus_client::{Client, ClientError, RemoteClient};
use serial_nexus_protocol::{
    DeviceSummary, ErrorCode, Event, PairedHost, PairingRole, PairingSecret, ShareBind,
};

use crate::input::{echo_key, map_key, Action, Key};
use crate::render::{hit_test, pairing_max_scroll, panes, settings_hit_test, Hit, SettingsHit};
use crate::settings::{SettingsStore, TerminalSettings};
use crate::state::{
    short_holder, AppState, ConnectForm, DeviceEntry, EndpointId, Focus, Notice, SessionKey,
    SettingsRow, SidebarButton, View, LOCAL_ENDPOINT,
};

/// Two clicks on the same cell within this long count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What the first click of a possible double-click landed on. It is kept by
/// identity, not by cell: the first click can move the sidebar's viewport, so
/// hit-testing the second click at the same cell would find another row.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ClickTarget {
    /// A device by endpoint and path. The endpoint keeps equal paths on two
    /// hosts apart, so a double-click never opens the wrong one.
    Device(EndpointId, String),
    Share,
    Connect,
    Settings,
}

struct LastClick {
    at: Instant,
    column: u16,
    row: u16,
    target: Option<ClickTarget>,
}

/// One connected remote host. The link's own client and event stream keep its
/// sessions and failures apart from every other host.
struct RemoteLink {
    id: EndpointId,
    client: RemoteClient,
    events: Receiver<Event>,
    expected_closes: HashSet<u64>,
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
    /// Remote hosts this terminal connected to, each with its own transport.
    remotes: Vec<RemoteLink>,
    /// The next endpoint number a remote link gets.
    next_endpoint: u64,
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
            remotes: Vec::new(),
            next_endpoint: 1,
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

    /// Re-read every connected host's device list. A remote host that fails
    /// is dropped with its own sessions only; local devices and other remote
    /// hosts keep working.
    pub fn refresh_devices(&mut self) {
        match self.client.list_devices() {
            Ok(result) => {
                let entries: Vec<DeviceEntry> =
                    result.devices.into_iter().map(device_entry).collect();
                self.state.set_endpoint_devices(LOCAL_ENDPOINT, entries);
                let detected = self.state.detected_local_identities();
                self.state.prune_share_checks(&detected);
                self.state.sync_checked_identities();
            }
            Err(error) => self.state.notice = Some(Notice::error(error.to_string())),
        }
        self.refresh_remotes();
        if self.state.sharing.active {
            self.sync_share_status();
        }
    }

    /// Re-list the devices of every remote host. A disconnected host is
    /// removed without touching any other endpoint's sessions.
    fn refresh_remotes(&mut self) {
        let mut failed = Vec::new();
        for link in &self.remotes {
            match link.client.list_devices() {
                Ok(result) => {
                    let entries: Vec<DeviceEntry> = result
                        .devices
                        .into_iter()
                        .map(|device| device_entry_on(link.id, device))
                        .collect();
                    self.state.set_endpoint_devices(link.id, entries);
                }
                Err(error) => failed.push((link.id, error)),
            }
        }
        for (id, error) in failed {
            self.drop_remote(id, &format!("remote host unreachable: {error}"));
        }
    }

    /// Remove one remote host: its sessions, its devices and its link. Other
    /// endpoints are untouched.
    fn drop_remote(&mut self, id: EndpointId, reason: &str) {
        self.state.remove_endpoint_sessions(id);
        self.state.devices.retain(|device| device.endpoint != id);
        self.state.remote_labels.remove(&id);
        self.remotes.retain(|link| link.id != id);
        // An open Connect form belongs to a host that is not connected yet, so
        // an unrelated host going away must not close it (and redirect the
        // credentials being typed into the shown device's stream).
        self.state.notice = Some(Notice::error(reason.to_string()));
    }

    /// Drain live events from every host into the state. Events carry their
    /// endpoint, so two hosts with equal device ids never cross streams.
    pub fn pump(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(LOCAL_ENDPOINT, event, None);
        }
        let mut overflowed = Vec::new();
        let pending: Vec<(EndpointId, Vec<Event>)> = self
            .remotes
            .iter()
            .map(|link| {
                let mut events = Vec::new();
                loop {
                    match link.events.try_recv() {
                        Ok(event) => events.push(event),
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            overflowed.push(link.id);
                            break;
                        }
                    }
                }
                (link.id, events)
            })
            .collect();
        for (id, events) in pending {
            for event in events {
                self.handle_event(id, event, Some(id));
            }
        }
        for id in overflowed {
            if self.remotes.iter().any(|link| link.id == id) {
                self.drop_remote(id, "remote host disconnected: its event queue overflowed");
            }
        }
    }

    fn handle_event(&mut self, endpoint: EndpointId, event: Event, remote: Option<EndpointId>) {
        let expected = match remote {
            None => &mut self.expected_closes,
            Some(id) => match self.remotes.iter_mut().find(|link| link.id == id) {
                Some(link) => &mut link.expected_closes,
                None => return,
            },
        };
        if let Event::DeviceClosed { device_id, .. } = &event {
            if expected.remove(device_id) {
                self.state.remove_session(SessionKey {
                    endpoint,
                    device_id: *device_id,
                });
                return;
            }
        }
        if matches!(
            &event,
            Event::TransportClosed { .. } | Event::CoreShutdown { .. }
        ) {
            if let Some(id) = remote {
                let reason = match &event {
                    Event::TransportClosed { reason } => reason.clone(),
                    Event::CoreShutdown { reason } => reason.clone(),
                    _ => String::new(),
                };
                self.drop_remote(id, &format!("remote host disconnected: {reason}"));
                return;
            }
            // The core ends a sharing session when its owner connection goes
            // away; there is no listener or credential left to show.
            self.state.sharing.clear_session();
            self.state.close_pairing_view();
        }
        self.state.apply_endpoint_event(endpoint, &event);
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
            Action::OpenSelected => self.activate_selected(),
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
            Action::SettingsBeginEdit => {
                self.state.begin_settings_edit();
            }
            Action::SettingsEditChar(ch) => self.state.settings_edit_push(ch),
            Action::SettingsEditBackspace => self.state.settings_edit_backspace(),
            Action::SettingsEditDone => self.state.end_settings_edit(),
            Action::SettingsSave => self.save_settings(),
            Action::SettingsCancel => self.state.cancel_settings(),
            Action::ShareToggle => self.toggle_sharing(),
            Action::OpenPairing => self.open_pairing(),
            Action::ConnectRemote => self.start_connect(),
            Action::ConnectChar(ch) => self.connect_push(ch),
            Action::ConnectBackspace => self.connect_backspace(),
            Action::ConnectNext(delta) => self.connect_next_field(delta),
            Action::ConnectSubmit => self.submit_connect(),
            Action::ConnectCancel => self.leave_connect(),
            Action::LeavePairing => self.leave_pairing(),
            Action::PairingScroll(delta) => self.scroll_pairing(delta),
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
        let key = SessionKey {
            endpoint: session.endpoint,
            device_id: session.device_id,
        };
        if !self.state.settings.scrollback {
            self.state.notice = Some(Notice::error("scrollback is off in the settings"));
            return;
        }
        self.state.show_device(key);
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

    /// Show one attached session (a test seam for a user switching streams).
    pub fn show_session_now(&mut self, key: SessionKey) {
        self.focus_session(key);
    }

    fn focus_session(&mut self, key: SessionKey) {
        // A device opened or joined by mouse or keyboard is the surface the
        // human now works on; a baud prompt for another row must not keep the
        // keys.
        self.state.prompt = None;
        self.state.show_device(key);
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
            Some(Hit::Share) => {
                self.state.select_index(self.state.devices.len());
                self.toggle_sharing();
                return;
            }
            Some(Hit::Connect) => {
                self.state
                    .select_index(self.state.devices.len().saturating_add(1));
                self.start_connect();
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
                    Some(SettingsHit::Flag(index)) if index < self.state.settings_row_count() => {
                        self.state.settings_row = index;
                        self.state.end_settings_edit();
                        match self.state.settings_kind(index) {
                            Some(SettingsRow::Device(_))
                            | Some(SettingsRow::LocalEcho)
                            | Some(SettingsRow::Scrollback) => {
                                self.state.toggle_draft();
                            }
                            Some(SettingsRow::ShareAddress | SettingsRow::SharePort) => {
                                self.state.begin_settings_edit();
                            }
                            None => {}
                        }
                    }
                    Some(SettingsHit::Save) => self.save_settings(),
                    Some(SettingsHit::Cancel) => self.state.cancel_settings(),
                    Some(SettingsHit::Flag(_)) | None => {}
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
                    // A device needs the second click to open it, and Settings
                    // is idempotent. Share and Connect acted on the first
                    // click; running them again would undo or reset it.
                    if !matches!(
                        target,
                        Some(ClickTarget::Share) | Some(ClickTarget::Connect)
                    ) {
                        self.activate_target(target);
                    }
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
                .map(|device| ClickTarget::Device(device.endpoint, device.path.clone())),
            Some(Hit::Share) => Some(ClickTarget::Share),
            Some(Hit::Connect) => Some(ClickTarget::Connect),
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
            Some(ClickTarget::Device(endpoint, path)) => {
                self.state.prompt = None;
                self.state.focus = Focus::Sidebar;
                if let Some(index) = self
                    .state
                    .devices
                    .iter()
                    .position(|device| device.endpoint == endpoint && device.path == path)
                {
                    self.state.select_index(index);
                    self.open_selected();
                }
            }
            Some(ClickTarget::Settings) => self.open_settings(),
            Some(ClickTarget::Share) => self.toggle_sharing(),
            Some(ClickTarget::Connect) => self.start_connect(),
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

    /// Activate the highlighted sidebar row: a device opens or joins, a button
    /// does what it says.
    fn activate_selected(&mut self) {
        match self.state.selected_button() {
            Some(SidebarButton::Share) => self.toggle_sharing(),
            Some(SidebarButton::Connect) => self.start_connect(),
            Some(SidebarButton::Settings) => self.open_settings(),
            None => self.open_selected(),
        }
    }

    /// Save the draft settings and checklist, then keep the draft and go back
    /// to the shown device's live stream.
    ///
    /// The draft becomes the settings in effect only after both the core
    /// accepted the checklist and the file was written. A refused checklist
    /// change or a failed write leaves the options as they were and stays in
    /// the settings, so nothing is claimed that did not happen.
    fn save_settings(&mut self) {
        self.state.end_settings_edit();
        let address = self.state.draft.share_address.trim().to_string();
        let address: IpAddr = match address.parse() {
            Ok(address) => address,
            Err(_) => {
                self.state.notice = Some(Notice::error(
                    "settings not saved: the share address is not an IPv4 or IPv6 address",
                ));
                return;
            }
        };
        let port: u16 = match self.state.port_text.trim().parse() {
            Ok(port) => port,
            Err(_) => {
                self.state.notice = Some(Notice::error(
                    "settings not saved: the share port is not a number from 0 to 65535",
                ));
                return;
            }
        };
        if let Err(message) = ShareBind::new(address, port).validate() {
            self.state.notice = Some(Notice::error(format!("settings not saved: {message}")));
            return;
        }
        self.state.draft.share_address = address.to_string();
        self.state.draft.share_port = port;

        // A replacement at a reused path must be checked again; stale UI state
        // is never turned into an authorization.
        self.state.drop_replaced_checks();

        let draft = self.state.draft.clone();
        let draft_checked = self.state.draft_checked.clone();
        let Some(store) = self.settings_store.clone() else {
            self.state.notice = Some(Notice::error("settings not saved: no settings file"));
            return;
        };
        let active = self.state.sharing.active;
        let desired: Vec<String> = draft_checked.iter().cloned().collect();
        let previous_settings = self.state.settings.clone();
        let previous_file = store.snapshot();

        // Every checked device needs the identity the owner selected. Without
        // one the core would fall back to the current node at that path, which
        // could be a replacement; refuse instead of authorizing it.
        let expected = self.expected_identities(&desired, &self.state.draft_identities);
        if active && expected.len() != desired.len() {
            self.state
                .draft_checked
                .retain(|path| expected.contains_key(path));
            self.state
                .draft_identities
                .retain(|path, _| self.state.draft_checked.contains(path));
            self.state.notice = Some(Notice::error(
                "settings not saved: a checked device has no captured identity; refresh and check it again",
            ));
            return;
        }

        // Write the file before touching the core: a failed save grants and
        // revokes nothing. A later core failure restores the file instead.
        if let Err(error) = store.save(&draft) {
            self.state.draft_checked = draft_checked;
            self.state.notice = Some(Notice::error(format!("settings not saved: {error}")));
            return;
        }

        if active {
            match self
                .client
                .replace_shared_devices_expected(desired.clone(), expected)
            {
                Ok(snapshot) => self.state.apply_sharing_snapshot(snapshot),
                Err(error) => {
                    // A timeout is not proof of failure: the core may have
                    // applied the new list. Ask it; only a list that matches
                    // the draft counts as success.
                    let uncertain = matches!(
                        error,
                        ClientError::Timeout(_) | ClientError::WriteUncertain(_)
                    );
                    if uncertain {
                        self.sync_share_status();
                        if self.state.sharing.active
                            && self.state.sharing.checked() == draft_checked
                        {
                            self.state.commit_settings();
                            self.state.notice = Some(Notice::info("settings saved"));
                            return;
                        }
                    }
                    let restored = match &previous_file {
                        Ok(snapshot) => store.restore(snapshot.as_deref()).is_ok(),
                        Err(_) => store.save(&previous_settings).is_ok(),
                    };
                    self.sync_share_status();
                    self.state.notice = Some(Notice::error(if restored {
                        format!("settings not saved: {error}")
                    } else {
                        format!(
                            "settings not saved: {error}; the previous settings could not be restored"
                        )
                    }));
                    return;
                }
            }
        }

        self.state.commit_settings();
        self.state.notice = Some(Notice::info("settings saved"));
    }

    pub fn toggle_sharing(&mut self) {
        if self.state.sharing.active {
            self.stop_sharing();
        } else {
            self.start_sharing();
        }
    }

    /// Turn sharing on deliberately. At least one device must be checked and
    /// the saved bind address must be one concrete IPv4 or IPv6 address; a
    /// bind failure is shown and leaves sharing off.
    pub fn start_sharing(&mut self) {
        if self.state.sharing.active {
            self.state.notice = Some(Notice::info("sharing is already on"));
            return;
        }
        if self.refuse_if_unsaved_settings() {
            return;
        }
        self.state.drop_replaced_checks();
        self.state.sync_checked_identities();
        let detected = self.state.detected_local_identities();
        let missing: Vec<String> = self
            .state
            .sharing
            .pending_checked
            .iter()
            .filter(|path| !detected.contains_key(path.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            for path in &missing {
                self.state.sharing.pending_checked.remove(path);
                self.state.draft_checked.remove(path);
                self.state.sharing.pending_identities.remove(path);
                self.state.draft_identities.remove(path);
            }
            self.state.notice = Some(Notice::error(format!(
                "sharing not started: no captured identity for {}; refresh and check it again",
                missing.join(", ")
            )));
            return;
        }
        let devices: Vec<String> = self.state.sharing.pending_checked.iter().cloned().collect();
        if devices.is_empty() {
            self.state.notice = Some(Notice::error(
                "check at least one device in Settings before starting to share",
            ));
            return;
        }
        let bind = match self.share_bind() {
            Ok(bind) => bind,
            Err(message) => {
                self.state.notice = Some(Notice::error(format!("sharing not started: {message}")));
                return;
            }
        };
        let expected = self.expected_identities(&devices, &self.state.sharing.pending_identities);
        match self.client.start_sharing_expected(bind, devices, expected) {
            Ok(info) => {
                self.state.sharing.active = true;
                self.state.sharing.pairing = Some(info);
                self.sync_share_status();
                self.state.view = View::Pairing;
                self.state.focus = Focus::Right;
                self.state.prompt = None;
                self.state.pairing_scroll = 0;
                self.state.notice = Some(Notice::info(
                    "sharing on — the pairing view shows the credentials",
                ));
            }
            Err(error) => {
                if matches!(
                    error,
                    ClientError::Timeout(_) | ClientError::WriteUncertain(_)
                ) {
                    // The core may have accepted ShareStart before the answer
                    // was lost. Keep the stop control available and ask the
                    // core what is really true instead of claiming it is off.
                    self.state.sharing.active = true;
                    self.sync_share_status();
                    if self.state.sharing.active {
                        self.state.notice = Some(Notice::error(format!(
                            "sharing start is unknown: {error}; sharing may be on — press s to stop it"
                        )));
                    } else {
                        self.state.notice =
                            Some(Notice::error(format!("sharing did not start: {error}")));
                    }
                } else {
                    self.state.sharing.clear_session();
                    self.state.notice =
                        Some(Notice::error(format!("sharing not started: {error}")));
                }
            }
        }
    }

    /// The recorded identity per path, for the paths still being granted.
    fn expected_identities(
        &self,
        paths: &[String],
        identities: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        paths
            .iter()
            .filter_map(|path| identities.get(path).map(|id| (path.clone(), id.clone())))
            .collect()
    }

    /// The saved sharing endpoint, validated as one concrete address.
    fn share_bind(&self) -> Result<ShareBind, String> {
        let address: IpAddr = self
            .state
            .settings
            .share_address
            .trim()
            .parse()
            .map_err(|_| {
                "the share address is not an IPv4 or IPv6 address (set it in Settings)".to_string()
            })?;
        let bind = ShareBind::new(address, self.state.settings.share_port);
        bind.validate()?;
        Ok(bind)
    }

    /// Stop sharing, which revokes both credentials and disconnects every
    /// remote client. The checked selection stays for this launch only.
    pub fn stop_sharing(&mut self) {
        if !self.state.sharing.active {
            self.state.notice = Some(Notice::info("sharing is already off"));
            return;
        }
        match self.client.stop_sharing() {
            Ok(()) => {
                self.state.sharing.clear_session();
                if self.state.view == View::Pairing {
                    self.show_live_stream();
                }
                self.state.notice = Some(Notice::info("sharing stopped"));
            }
            Err(error) => {
                self.state.notice = Some(Notice::error(format!("sharing not stopped: {error}")));
                self.sync_share_status();
            }
        }
    }

    /// Show the owner's pairing information. Only the terminal that started
    /// the session may see it, and only while the session is running.
    pub fn open_pairing(&mut self) {
        if !self.state.sharing.active {
            self.state.notice = Some(Notice::error(
                "sharing is off: press s or use the Share button to start it",
            ));
            return;
        }
        if self.refuse_if_unsaved_settings() {
            return;
        }
        self.state.prompt = None;
        self.state.view = View::Pairing;
        self.state.focus = Focus::Right;
        self.state.pairing_scroll = 0;
    }

    fn leave_pairing(&mut self) {
        self.show_live_stream();
    }

    fn scroll_pairing(&mut self, delta: i32) {
        let next = self.state.pairing_scroll as i32 + delta;
        let max = pairing_max_scroll(&self.state);
        self.state.pairing_scroll = next.clamp(0, max as i32) as usize;
    }

    /// Ask a remote host for its address, port and pairing information. The
    /// form's values stay in memory; nothing is written to the settings file
    /// or sent to a serial device.
    pub fn start_connect(&mut self) {
        if self.refuse_if_unsaved_settings() {
            return;
        }
        self.state.prompt = None;
        self.state.connect = Some(ConnectForm::new());
        self.state.view = View::Connect;
        self.state.focus = Focus::Right;
    }

    fn leave_connect(&mut self) {
        self.state.connect = None;
        self.show_live_stream();
        self.state.focus = Focus::Sidebar;
    }

    pub fn connect_push(&mut self, ch: char) {
        if let Some(form) = &mut self.state.connect {
            form.push(ch);
        }
    }

    pub fn connect_backspace(&mut self) {
        if let Some(form) = &mut self.state.connect {
            form.backspace();
        }
    }

    pub fn connect_next_field(&mut self, delta: i32) {
        if let Some(form) = &mut self.state.connect {
            form.next_field(delta);
        }
    }

    /// Authenticate the encrypted host and add its approved devices. A wrong
    /// certificate, address, port or credential keeps the form open with the
    /// reason and never silently trusts the host.
    pub fn submit_connect(&mut self) {
        let Some(mut form) = self.state.connect.take() else {
            return;
        };
        let result = self.connect_remote(&form);
        match result {
            Ok((id, label)) => {
                self.state.remote_labels.insert(id, label.clone());
                self.state.connect = None;
                self.show_live_stream();
                self.state.focus = Focus::Sidebar;
                self.refresh_remotes();
                self.state.notice = Some(Notice::info(format!(
                    "connected to {label}: its checked devices are in the sidebar"
                )));
            }
            Err(error) => {
                form.error = Some(error);
                self.state.connect = Some(form);
            }
        }
    }

    /// Dial and authenticate one host, then register its endpoint. Every
    /// failure leaves nothing behind.
    fn connect_remote(&mut self, form: &ConnectForm) -> Result<(EndpointId, String), String> {
        let address: IpAddr = form
            .address
            .trim()
            .parse()
            .map_err(|_| "the host address is not an IPv4 or IPv6 address".to_string())?;
        let port: u16 = form
            .port
            .trim()
            .parse()
            .map_err(|_| "the port is not a number from 1 to 65535".to_string())?;
        if port == 0 {
            return Err("the port must be between 1 and 65535".to_string());
        }
        let fingerprint = form.fingerprint.trim().to_ascii_lowercase();
        if fingerprint.len() != 64 || !fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err("the host fingerprint is not a SHA-256 hex value".to_string());
        }
        if form.ca.trim().is_empty() {
            return Err("enter the host's CA certificate from its pairing view".to_string());
        }
        if form.credential.trim().is_empty() {
            return Err("enter the host's human pairing credential".to_string());
        }
        let host_name = address.to_string();
        let paired = PairedHost {
            address: SocketAddr::new(address, port),
            host_name,
            ca_cert_der_b64: form.ca.trim().to_string(),
            host_cert_fingerprint_sha256: fingerprint,
        };
        let secret = PairingSecret {
            role: PairingRole::Human,
            credential: form.credential.trim().to_string(),
        };
        let check = serial_nexus_client::RemoteClient::connect(
            &paired,
            &secret,
            Some("serial-nexus-terminal".to_string()),
            Duration::from_secs(5),
        )
        .map_err(|error| format!("not connected: {error}"))?;
        let id = EndpointId(self.next_endpoint);
        self.next_endpoint += 1;
        let label = format!("Remote {}", paired.address);
        let events = check.subscribe_bounded(1024);
        self.remotes.push(RemoteLink {
            id,
            client: check,
            events,
            expected_closes: HashSet::new(),
        });
        Ok((id, label))
    }

    fn remote_client(&self, endpoint: EndpointId) -> Option<&RemoteClient> {
        self.remotes
            .iter()
            .find(|link| link.id == endpoint)
            .map(|link| &link.client)
    }

    /// Resolve one endpoint to its transport. `None` means this machine's
    /// client; an unknown remote endpoint is an error everywhere, never a
    /// silent fallback to the local core.
    fn endpoint_client(&self, endpoint: EndpointId) -> Result<Option<&RemoteClient>, ClientError> {
        if endpoint == LOCAL_ENDPOINT {
            return Ok(None);
        }
        self.remote_client(endpoint).map(Some).ok_or_else(|| {
            ClientError::Disconnected("the remote host is no longer connected".into())
        })
    }

    fn open_on(
        &self,
        endpoint: EndpointId,
        path: &str,
        baud: Option<u32>,
    ) -> Result<serial_nexus_protocol::OpenResult, ClientError> {
        match self.endpoint_client(endpoint)? {
            None => self.client.open(path, baud, false),
            Some(remote) => remote.open(path, baud, false),
        }
    }

    /// Ask the core for the running session's state and take its answer as
    /// authoritative. A checked device the core no longer lists (unplugged,
    /// replaced at the same path, or revoked) loses its checkbox.
    fn sync_share_status(&mut self) {
        match self.client.sharing_status() {
            Ok(snapshot) => {
                let before = self.state.sharing.checked();
                let after: BTreeSet<String> = snapshot
                    .checked
                    .iter()
                    .map(|device| device.path.clone())
                    .collect();
                let lost: Vec<String> = before.difference(&after).cloned().collect();
                self.state.apply_sharing_snapshot(snapshot);
                if !lost.is_empty() {
                    self.state.notice = Some(Notice::error(format!(
                        "no longer shared: {} — check it again in Settings",
                        lost.join(", ")
                    )));
                }
            }
            Err(error) => {
                // A timeout says only that we do not know the state. Keep the
                // owner view (and the credentials in memory) so another `s`
                // still sends ShareStop; never claim the listener is gone.
                let ended = matches!(
                    error,
                    ClientError::Disconnected(_) | ClientError::WriteUncertain(_)
                ) || matches!(
                    error.code(),
                    Some(
                        ErrorCode::NotFound | ErrorCode::PermissionDenied | ErrorCode::ShuttingDown
                    )
                );
                if ended {
                    self.state.sharing.clear_session();
                    self.state.close_pairing_view();
                    self.state.notice = Some(Notice::error(format!("sharing stopped: {error}")));
                } else {
                    self.state.notice = Some(Notice::error(format!(
                        "sharing status is unknown: {error}; sharing may still be on — press s to stop it"
                    )));
                }
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
        let endpoint = device.endpoint;
        if let Some(session) = self.state.highlighted_session() {
            let key = SessionKey {
                endpoint: session.endpoint,
                device_id: session.device_id,
            };
            self.focus_session(key);
            self.state.notice = Some(Notice::info("already attached"));
            return;
        }
        if device.open {
            match self.open_endpoint(endpoint, &path, None) {
                Ok(_) => {}
                Err(error) => self.state.notice = Some(Notice::error(error.to_string())),
            }
        } else if !self.state.begin_baud_prompt() {
            self.state.notice = Some(Notice::error("cannot open the highlighted device"));
        }
    }

    /// Open or join a device on this machine and show its live stream.
    pub fn open_path(&mut self, path: &str, baud: Option<u32>) -> Result<u64, ClientError> {
        self.open_endpoint(LOCAL_ENDPOINT, path, baud)
            .map(|key| key.device_id)
    }

    /// Open or join a device on one host and show its live stream. Sessions
    /// start empty and are fed by live events only, so output races never
    /// duplicate. The key includes the endpoint, so equal device ids and paths
    /// on two hosts stay separate.
    pub fn open_endpoint(
        &mut self,
        endpoint: EndpointId,
        path: &str,
        baud: Option<u32>,
    ) -> Result<SessionKey, ClientError> {
        let opened = self.open_on(endpoint, path, baud)?;
        if endpoint == LOCAL_ENDPOINT {
            // Rejoining a device (even the same id, while another client kept
            // it open) must not leave an old close suppression behind.
            self.expected_closes.remove(&opened.device_id);
        } else if let Some(link) = self.remotes.iter_mut().find(|link| link.id == endpoint) {
            link.expected_closes.remove(&opened.device_id);
        }
        let key = SessionKey {
            endpoint,
            device_id: opened.device_id,
        };
        self.state.add_endpoint_session(
            endpoint,
            opened.device_id,
            opened.port.clone(),
            opened.baud,
            opened.control.holder.clone(),
        );
        self.focus_session(key);
        self.state.notice = Some(Notice::info(format!(
            "opened {} at {} baud",
            opened.port, opened.baud
        )));
        self.refresh_devices();
        Ok(key)
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
            Ok(baud) => match self.open_endpoint(prompt.endpoint, &prompt.path, Some(baud)) {
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
        let key = SessionKey {
            endpoint: session.endpoint,
            device_id: session.device_id,
        };
        let path = session.path.clone();
        if key.endpoint == LOCAL_ENDPOINT {
            self.expected_closes.insert(key.device_id);
        } else if let Some(link) = self.remotes.iter_mut().find(|link| link.id == key.endpoint) {
            link.expected_closes.insert(key.device_id);
        }
        let closed = match self.endpoint_client(key.endpoint) {
            Ok(None) => self.client.close(key.device_id),
            Ok(Some(remote)) => remote.close(key.device_id),
            Err(error) => Err(error),
        };
        match closed {
            Ok(()) => {
                self.state.remove_session(key);
                self.state.notice = Some(Notice::info(format!("disconnected {path}")));
            }
            Err(error) => {
                if error.is_device_gone() {
                    self.state.remove_session(key);
                }
                self.state.notice =
                    Some(Notice::error(format!("disconnect {path} failed: {error}")));
            }
        }
        self.refresh_devices();
    }

    /// Write human input to the shown device without taking AI control. With
    /// `local_echo` on, the typed text is shown locally too; it is added only
    /// after the write was accepted, so a refused or uncertain write leaves no
    /// ghost and is never retried.
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
        let session_key = SessionKey {
            endpoint: session.endpoint,
            device_id: session.device_id,
        };
        let path = session.path.clone();
        let endpoint = session.endpoint;
        let written = match self.endpoint_client(endpoint) {
            Ok(None) => self.client.write_bytes(session_key.device_id, bytes, None),
            Ok(Some(remote)) => remote.write_bytes(session_key.device_id, bytes, None),
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            if error.is_device_gone() {
                self.state.remove_session(session_key);
            }
            let retried = if error.is_write_uncertain() {
                " — the bytes may have reached it and were not retried"
            } else {
                ""
            };
            self.state.notice = Some(Notice::error(format!("write to {path}{retried}: {error}")));
            return;
        }
        let local_echo = self.state.settings.local_echo;
        if local_echo {
            if let (Some(echo), Some(session)) =
                (echo_key(key), self.state.sessions.get_mut(&session_key))
            {
                session.screen.echo(&echo);
            }
        }
    }

    /// Detach every device and stop sharing. Called on exit and after an
    /// event-loop error, so this terminal never leaves a listener or a remote
    /// session behind.
    pub fn shutdown(&mut self) {
        if self.state.sharing.active {
            let _ = self.client.stop_sharing();
            self.state.sharing.clear_session();
        }
        for key in self.state.all_sessions() {
            match self.endpoint_client(key.endpoint) {
                Ok(None) => {
                    let _ = self.client.close(key.device_id);
                }
                Ok(Some(remote)) => {
                    let _ = remote.close(key.device_id);
                }
                Err(_) => {}
            }
            self.state.remove_session(key);
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
    device_entry_on(LOCAL_ENDPOINT, device)
}

fn device_entry_on(endpoint: EndpointId, device: DeviceSummary) -> DeviceEntry {
    DeviceEntry {
        endpoint,
        path: device.path,
        description: device.description,
        identity: device.identity,
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
