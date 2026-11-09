//! Key mapping for the two panes. Kept free of crossterm types so the input
//! seam can be tested without a terminal.

use crate::state::{AppState, Focus, Overlay};

/// A key the TUI understands, independent of the terminal library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    Ctrl(char),
}

/// What a key asks the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    Select(i32),
    OpenSelected,
    DisconnectSelected,
    StartBaudPrompt,
    ToggleFocus,
    ToggleHelp,
    ToggleSettings,
    CloseOverlay,
    SettingsMove(i32),
    SettingsToggle,
    PromptChar(char),
    PromptBackspace,
    PromptSubmit,
    PromptCancel,
    /// Bytes for the selected device. Never drawn locally.
    Write(Vec<u8>),
    Ignore,
}

/// Map a key to an action for the current state.
pub fn map_key(state: &AppState, key: Key) -> Action {
    if matches!(key, Key::Ctrl('q') | Key::F(10)) {
        return Action::Quit;
    }
    if state.prompt.is_some() {
        return match key {
            Key::Char(ch) => Action::PromptChar(ch),
            Key::Enter => Action::PromptSubmit,
            Key::Esc => Action::PromptCancel,
            Key::Backspace => Action::PromptBackspace,
            _ => Action::Ignore,
        };
    }
    if let Some(overlay) = state.overlay {
        return map_overlay_key(overlay, key);
    }
    // The panels are on function keys: a stream writes every printable key,
    // so a letter or `?` must never become a panel instead of device input.
    if key == Key::F(1) {
        return Action::ToggleHelp;
    }
    if key == Key::F(2) {
        return Action::ToggleSettings;
    }
    if key == Key::Tab {
        return Action::ToggleFocus;
    }
    match state.focus {
        Focus::Devices => match key {
            Key::Up | Key::Char('k') => Action::Select(-1),
            Key::Down | Key::Char('j') => Action::Select(1),
            Key::PageUp => Action::Select(-10),
            Key::PageDown => Action::Select(10),
            Key::Enter | Key::Char('o') => Action::OpenSelected,
            Key::Char('d') => Action::DisconnectSelected,
            Key::Char('b') => Action::StartBaudPrompt,
            Key::Right => Action::ToggleFocus,
            _ => Action::Ignore,
        },
        Focus::Stream => match encode_key(key) {
            Some(bytes) => Action::Write(bytes),
            None => Action::Ignore,
        },
    }
}

/// Keys a panel takes while it is open. The panes take none of them.
fn map_overlay_key(overlay: Overlay, key: Key) -> Action {
    match overlay {
        Overlay::Help => match key {
            Key::Esc | Key::F(1) => Action::CloseOverlay,
            _ => Action::Ignore,
        },
        Overlay::Settings => match key {
            Key::Esc | Key::F(2) => Action::CloseOverlay,
            Key::Up | Key::Char('k') => Action::SettingsMove(-1),
            Key::Down | Key::Char('j') => Action::SettingsMove(1),
            Key::Char(' ') | Key::Enter | Key::Left | Key::Right => Action::SettingsToggle,
            _ => Action::Ignore,
        },
    }
}

/// Bytes a stream-focused key sends to the selected device.
///
/// This is ordinary terminal input: arrows are ANSI sequences, Enter is CR and
/// Backspace is DEL. Human writes are echoed locally only when the `local_echo`
/// setting is on (see [`echo_key`]).
pub fn encode_key(key: Key) -> Option<Vec<u8>> {
    let bytes: Vec<u8> = match key {
        Key::Char(ch) => {
            let mut buffer = [0u8; 4];
            ch.encode_utf8(&mut buffer).as_bytes().to_vec()
        }
        Key::Enter => vec![b'\r'],
        Key::Esc => vec![0x1b],
        Key::Tab => vec![b'\t'],
        Key::BackTab => b"\x1b[Z".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Right => b"\x1b[C".to_vec(),
        Key::Left => b"\x1b[D".to_vec(),
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::F(number) => match number {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => return None,
        },
        Key::Ctrl(ch) => {
            let upper = ch.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                vec![(upper as u8) & 0x1f]
            } else {
                return None;
            }
        }
    };
    Some(bytes)
}

/// Bytes to echo locally for a key typed in the stream.
///
/// Echo is deliberately narrower than [`encode_key`]: only printable text and
/// the editing keys a person expects to see are echoed. Escape sequences for
/// arrows and function keys are not, because echoing them would move the local
/// cursor instead of showing what was typed.
pub fn echo_key(key: Key) -> Option<Vec<u8>> {
    match key {
        Key::Char(ch) => {
            let mut buffer = [0u8; 4];
            Some(ch.encode_utf8(&mut buffer).as_bytes().to_vec())
        }
        Key::Enter => Some(b"\r\n".to_vec()),
        Key::Tab => Some(b"\t".to_vec()),
        // Move back over the character and erase it, like a terminal does.
        Key::Backspace => Some(b"\x08 \x08".to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DeviceEntry;

    #[test]
    fn sidebar_keys_select_open_disconnect_and_prompt() {
        let mut state = AppState::new(20, 80);
        state.set_devices(vec![DeviceEntry::new("/dev/ttyUSB0")]);
        assert_eq!(map_key(&state, Key::Down), Action::Select(1));
        assert_eq!(map_key(&state, Key::Enter), Action::OpenSelected);
        assert_eq!(map_key(&state, Key::Char('d')), Action::DisconnectSelected);
        assert_eq!(map_key(&state, Key::Char('b')), Action::StartBaudPrompt);
        assert_eq!(map_key(&state, Key::Tab), Action::ToggleFocus);
        assert_eq!(map_key(&state, Key::Ctrl('q')), Action::Quit);
    }

    #[test]
    fn stream_keys_become_device_bytes() {
        let mut state = AppState::new(20, 80);
        state.focus = Focus::Stream;
        assert_eq!(
            map_key(&state, Key::Char('A')),
            Action::Write(b"A".to_vec())
        );
        assert_eq!(map_key(&state, Key::Enter), Action::Write(b"\r".to_vec()));
        assert_eq!(map_key(&state, Key::Up), Action::Write(b"\x1b[A".to_vec()));
        assert_eq!(map_key(&state, Key::Ctrl('c')), Action::Write(vec![0x03]));
        assert_eq!(map_key(&state, Key::Tab), Action::ToggleFocus);
        assert_eq!(map_key(&state, Key::F(10)), Action::Quit);
    }

    #[test]
    fn prompt_takes_digits_enter_escape_backspace() {
        let mut state = AppState::new(20, 80);
        state.set_devices(vec![DeviceEntry::new("/dev/ttyUSB0")]);
        state.begin_baud_prompt();
        assert_eq!(map_key(&state, Key::Char('9')), Action::PromptChar('9'));
        assert_eq!(map_key(&state, Key::Enter), Action::PromptSubmit);
        assert_eq!(map_key(&state, Key::Esc), Action::PromptCancel);
        assert_eq!(map_key(&state, Key::Backspace), Action::PromptBackspace);
    }

    #[test]
    fn escape_sequence_encoding_is_standard() {
        assert_eq!(encode_key(Key::Esc), Some(b"\x1b".to_vec()));
        assert_eq!(encode_key(Key::Backspace), Some(vec![0x7f]));
        assert_eq!(encode_key(Key::F(1)), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode_key(Key::Ctrl('@')), None);
    }

    #[test]
    fn function_keys_open_the_panels_and_printables_stay_device_input() {
        let mut state = AppState::new(20, 80);
        state.focus = Focus::Stream;
        assert_eq!(map_key(&state, Key::F(1)), Action::ToggleHelp);
        assert_eq!(map_key(&state, Key::F(2)), Action::ToggleSettings);
        // Serial consoles need these: they must reach the device.
        assert_eq!(
            map_key(&state, Key::Char('?')),
            Action::Write(b"?".to_vec())
        );
        assert_eq!(
            map_key(&state, Key::Char('q')),
            Action::Write(b"q".to_vec())
        );
    }

    #[test]
    fn an_open_panel_takes_the_keys_and_escape_closes_it() {
        let mut state = AppState::new(20, 80);
        state.overlay = Some(Overlay::Settings);
        assert_eq!(map_key(&state, Key::Down), Action::SettingsMove(1));
        assert_eq!(map_key(&state, Key::Char('j')), Action::SettingsMove(1));
        assert_eq!(map_key(&state, Key::Char('k')), Action::SettingsMove(-1));
        assert_eq!(map_key(&state, Key::Char(' ')), Action::SettingsToggle);
        assert_eq!(map_key(&state, Key::Enter), Action::SettingsToggle);
        assert_eq!(map_key(&state, Key::Esc), Action::CloseOverlay);
        assert_eq!(map_key(&state, Key::F(2)), Action::CloseOverlay);
        // Typing into the device is off while a panel is open.
        assert_eq!(map_key(&state, Key::Char('x')), Action::Ignore);

        state.overlay = Some(Overlay::Help);
        assert_eq!(map_key(&state, Key::Esc), Action::CloseOverlay);
        assert_eq!(map_key(&state, Key::F(1)), Action::CloseOverlay);
        assert_eq!(map_key(&state, Key::Char('x')), Action::Ignore);
        // Quitting still works from a panel.
        assert_eq!(map_key(&state, Key::Ctrl('q')), Action::Quit);
    }

    #[test]
    fn echo_shows_printed_and_editing_keys_only() {
        assert_eq!(echo_key(Key::Char('A')), Some(b"A".to_vec()));
        assert_eq!(echo_key(Key::Enter), Some(b"\r\n".to_vec()));
        assert_eq!(echo_key(Key::Tab), Some(b"\t".to_vec()));
        assert_eq!(echo_key(Key::Backspace), Some(b"\x08 \x08".to_vec()));
        assert_eq!(echo_key(Key::Up), None);
        assert_eq!(echo_key(Key::Ctrl('c')), None);
    }
}
