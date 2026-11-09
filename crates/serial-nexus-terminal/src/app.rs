//! The event-driven app: client calls behind small state transitions that do
//! not depend on a real terminal.

use std::collections::HashSet;

use crossbeam_channel::Receiver;
use serial_nexus_client::{Client, ClientError};
use serial_nexus_protocol::{DeviceSummary, Event};

use crate::input::{echo_key, map_key, Action, Key};
use crate::settings::{SettingsStore, TerminalSettings};
use crate::state::{short_holder, AppState, DeviceEntry, Focus, Notice, Overlay};

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
        let mut app = Self {
            state,
            client,
            events,
            expected_closes: HashSet::new(),
            settings_store: store,
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

    /// Handle one key. Typing in the stream writes to the selected device and
    /// is echoed locally only when the `local_echo` setting is on.
    pub fn handle_key(&mut self, key: Key) {
        let action = map_key(&self.state, key);
        match action {
            Action::Quit => self.state.quit = true,
            Action::Select(delta) => self.state.select_delta(delta),
            Action::ToggleFocus => self.state.focus = self.state.focus.toggled(),
            Action::OpenSelected => self.open_selected(),
            Action::DisconnectSelected => self.disconnect_selected(),
            Action::StartBaudPrompt => self.start_baud_prompt(),
            Action::PromptChar(ch) => self.state.prompt_push(ch),
            Action::PromptBackspace => self.state.prompt_backspace(),
            Action::PromptCancel => self.state.prompt = None,
            Action::PromptSubmit => self.submit_baud_prompt(),
            Action::Write(bytes) => self.write_selected(&bytes, key),
            Action::ToggleHelp => self.toggle_overlay(Overlay::Help),
            Action::ToggleSettings => self.toggle_overlay(Overlay::Settings),
            Action::CloseOverlay => self.state.overlay = None,
            Action::SettingsMove(delta) => self.state.move_settings_row(delta),
            Action::SettingsToggle => self.toggle_setting(),
            Action::HelpScroll(delta) => {
                self.state.help_offset = (self.state.help_offset as i32 + delta).max(0) as usize;
            }
            Action::Ignore => {}
        }
    }

    /// Open a panel, or close it when the same one is already open.
    fn toggle_overlay(&mut self, overlay: Overlay) {
        self.state.overlay = (self.state.overlay != Some(overlay)).then_some(overlay);
        if self.state.overlay == Some(Overlay::Help) {
            self.state.help_offset = 0;
        }
    }

    /// Flip the selected setting and save it. A save failure is reported and
    /// leaves the preference in effect for this launch only.
    fn toggle_setting(&mut self) {
        let Some(row) = AppState::settings_row_name(self.state.settings_row) else {
            return;
        };
        let Some(value) = self.state.toggle_selected_setting() else {
            return;
        };
        let state = if value { "on" } else { "off" };
        self.state.notice = Some(match &self.settings_store {
            Some(store) => match store.save(&self.state.settings) {
                Ok(()) => Notice::info(format!("{row}: {state}")),
                Err(error) => Notice::error(format!("{row}: {state} (not saved: {error})")),
            },
            None => Notice::info(format!("{row}: {state} (this launch only)")),
        });
    }

    /// Open the selected device. A closed device asks for a baud rate first;
    /// an already-open device is joined at its active rate.
    pub fn open_selected(&mut self) {
        let Some(device) = self.state.selected_device() else {
            self.state.notice = Some(Notice::error("no device selected"));
            return;
        };
        if self.state.selected_session().is_some() {
            self.state.focus = Focus::Stream;
            self.state.notice = Some(Notice::info("already attached"));
            return;
        }
        let path = device.path.clone();
        if device.open {
            match self.open_path(&path, None) {
                Ok(_) => {}
                Err(error) => self.state.notice = Some(Notice::error(error.to_string())),
            }
        } else if !self.state.begin_baud_prompt() {
            self.state.notice = Some(Notice::error("cannot open the selected device"));
        }
    }

    /// Open or join `path` and focus its live stream. Sessions start empty and
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
        self.state.focus = Focus::Stream;
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
        if self.state.begin_baud_prompt() {
            return;
        }
        let Some(device) = self.state.selected_device() else {
            self.state.notice = Some(Notice::error("no device selected"));
            return;
        };
        if let Some(baud) = device.baud {
            self.state.notice = Some(Notice::info(format!(
                "already open at {baud} baud; Enter joins at the active rate"
            )));
        }
    }

    /// Explicitly detach the selected device. Other sessions stay connected.
    pub fn disconnect_selected(&mut self) {
        let Some(session) = self.state.selected_session() else {
            self.state.notice = Some(Notice::error("not attached to a device"));
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

    /// Write human input to the selected device without taking AI control.
    /// With `local_echo` on, the typed text is shown locally too; it is added
    /// only after the write was accepted, so a refused write leaves no ghost.
    pub fn write_selected(&mut self, bytes: &[u8], key: Key) {
        let Some(session) = self.state.selected_session() else {
            self.state.notice = Some(Notice::error(
                "no open device — press Tab and Enter to open one",
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
        if self.state.settings.local_echo {
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
