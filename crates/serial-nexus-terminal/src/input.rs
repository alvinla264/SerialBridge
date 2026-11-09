//! Key mapping for the two panes. Kept free of crossterm types so the input
//! seam can be tested without a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{AppState, View};

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
    /// Show the stream or the sidebar.
    ToggleView,
    PromptChar(char),
    PromptBackspace,
    PromptSubmit,
    PromptCancel,
    /// Bytes for the selected device. Never drawn locally unless local echo is
    /// on.
    Write(Vec<u8>),
    /// Open the settings that the sidebar button offers.
    OpenSettings,
    SettingsMove(i32),
    SettingsToggle,
    SettingsSave,
    SettingsCancel,
    Ignore,
}

/// Map a key to an action for the current state.
///
/// The stream is a transparent terminal: every key it can encode goes to the
/// device, `Tab` and `Ctrl+Q` included. Only `Ctrl+]` leaves it, which is why
/// the sidebar's `]` sends a literal `Ctrl+]`. The sidebar is the command
/// surface: `Ctrl+Q` quits there, and the settings button is a row of it.
pub fn map_key(state: &AppState, key: Key) -> Action {
    if state.prompt.is_some() {
        return match key {
            // The prompt belongs to the sidebar, so the sidebar's quit still
            // works while it is open.
            Key::Ctrl('q') | Key::F(10) if state.view == View::Devices => Action::Quit,
            Key::Char(ch) => Action::PromptChar(ch),
            Key::Enter => Action::PromptSubmit,
            Key::Esc => Action::PromptCancel,
            Key::Backspace => Action::PromptBackspace,
            _ => Action::Ignore,
        };
    }
    if state.view == View::Settings {
        return match key {
            Key::Ctrl('s') => Action::SettingsSave,
            Key::Esc => Action::SettingsCancel,
            Key::Tab | Key::Down | Key::Char('j') => Action::SettingsMove(1),
            Key::BackTab | Key::Up | Key::Char('k') => Action::SettingsMove(-1),
            Key::Char(' ') | Key::Enter => Action::SettingsToggle,
            _ => Action::Ignore,
        };
    }
    if key == Key::Ctrl(']') {
        return Action::ToggleView;
    }
    match state.view {
        View::Devices => match key {
            Key::Ctrl('q') | Key::F(10) => Action::Quit,
            // The sidebar can still feed a device: `]` is the literal
            // Ctrl+] the stream cannot send without leaving it, and Tab is
            // ordinary terminal input.
            Key::Char(']') => Action::Write(vec![0x1d]),
            Key::Tab => Action::Write(vec![b'\t']),
            Key::Up | Key::Char('k') => Action::Select(-1),
            Key::Down | Key::Char('j') => Action::Select(1),
            Key::PageUp => Action::Select(-10),
            Key::PageDown => Action::Select(10),
            Key::Home => Action::Select(i32::MIN),
            Key::End => Action::Select(i32::MAX),
            Key::Enter | Key::Char('o') => Action::OpenSelected,
            Key::Char('b') => Action::StartBaudPrompt,
            Key::Char('d') => Action::DisconnectSelected,
            _ => Action::Ignore,
        },
        View::Stream => match encode_key(key) {
            Some(bytes) => Action::Write(bytes),
            None => Action::Ignore,
        },
        View::Settings => Action::Ignore,
    }
}

/// Translate a terminal key event into the TUI's key.
///
/// Ctrl+] is the one key the terminal keeps. A terminal that is not speaking
/// the enhanced keyboard protocol sends it as the raw `0x1d` byte, which
/// crossterm decodes as Ctrl+5; the enhanced protocol sends `Ctrl+]` directly.
/// Both spell the same key here, so the toggle works either way.
pub fn translate(event: KeyEvent) -> Option<Key> {
    let control = event.modifiers.contains(KeyModifiers::CONTROL);
    match event.code {
        KeyCode::Char(']') if control => Some(Key::Ctrl(']')),
        KeyCode::Char('5') if control => Some(Key::Ctrl(']')),
        KeyCode::Char(ch) if control => Some(Key::Ctrl(ch.to_ascii_lowercase())),
        KeyCode::Char(ch) => Some(Key::Char(ch)),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::BackTab => Some(Key::BackTab),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Delete => Some(Key::Delete),
        KeyCode::Insert => Some(Key::Insert),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        KeyCode::PageUp => Some(Key::PageUp),
        KeyCode::PageDown => Some(Key::PageDown),
        KeyCode::F(number) => Some(Key::F(number)),
        _ => None,
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

    fn state_with_device() -> AppState {
        let mut state = AppState::new(20, 80);
        state.set_devices(vec![DeviceEntry::new("/dev/ttyUSB0")]);
        state
    }

    #[test]
    fn sidebar_keys_select_open_disconnect_and_prompt() {
        let state = state_with_device();
        assert_eq!(map_key(&state, Key::Down), Action::Select(1));
        assert_eq!(map_key(&state, Key::Enter), Action::OpenSelected);
        assert_eq!(map_key(&state, Key::Char('o')), Action::OpenSelected);
        assert_eq!(map_key(&state, Key::Char('d')), Action::DisconnectSelected);
        assert_eq!(map_key(&state, Key::Char('b')), Action::StartBaudPrompt);
        assert_eq!(map_key(&state, Key::Ctrl('q')), Action::Quit);
        assert_eq!(map_key(&state, Key::F(10)), Action::Quit);
    }

    #[test]
    fn ctrl_bracket_switches_views_from_either_side() {
        let mut state = state_with_device();
        assert_eq!(map_key(&state, Key::Ctrl(']')), Action::ToggleView);
        state.view = View::Stream;
        assert_eq!(map_key(&state, Key::Ctrl(']')), Action::ToggleView);
    }

    #[test]
    fn the_stream_is_transparent_for_tab_and_ctrl_q() {
        let mut state = state_with_device();
        state.view = View::Stream;
        assert_eq!(map_key(&state, Key::Tab), Action::Write(b"\t".to_vec()));
        assert_eq!(
            map_key(&state, Key::Ctrl('q')),
            Action::Write(vec![0x11]),
            "Ctrl+Q is the device's XON/XOFF, not a quit"
        );
        assert_eq!(
            map_key(&state, Key::Char('?')),
            Action::Write(b"?".to_vec())
        );
        assert_eq!(
            map_key(&state, Key::Char('A')),
            Action::Write(b"A".to_vec())
        );
        assert_eq!(map_key(&state, Key::Enter), Action::Write(b"\r".to_vec()));
        assert_eq!(map_key(&state, Key::Up), Action::Write(b"\x1b[A".to_vec()));
        assert_eq!(
            map_key(&state, Key::F(10)),
            Action::Write(b"\x1b[21~".to_vec())
        );
    }

    #[test]
    fn the_sidebar_sends_the_literal_ctrl_bracket_and_tab() {
        let state = state_with_device();
        assert_eq!(map_key(&state, Key::Char(']')), Action::Write(vec![0x1d]));
        assert_eq!(map_key(&state, Key::Tab), Action::Write(b"\t".to_vec()));
        // Printable commands are still commands here.
        assert_eq!(map_key(&state, Key::Char('b')), Action::StartBaudPrompt);
    }

    #[test]
    fn prompt_takes_digits_enter_escape_backspace() {
        let mut state = state_with_device();
        state.begin_baud_prompt();
        assert_eq!(map_key(&state, Key::Char('9')), Action::PromptChar('9'));
        assert_eq!(map_key(&state, Key::Enter), Action::PromptSubmit);
        assert_eq!(map_key(&state, Key::Esc), Action::PromptCancel);
        assert_eq!(map_key(&state, Key::Backspace), Action::PromptBackspace);
        // The prompt owns the keys: Ctrl+] cannot pull the view away.
        assert_eq!(map_key(&state, Key::Ctrl(']')), Action::Ignore);
    }

    #[test]
    fn ctrl_bracket_arrives_as_both_terminal_encodings() {
        use crossterm::event::KeyEvent;
        // The enhanced keyboard protocol names the key.
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            Some(Key::Ctrl(']'))
        );
        // A plain terminal sends the raw 0x1d byte, which crossterm decodes
        // as Ctrl+5: the same key has to come out here.
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL)),
            Some(Key::Ctrl(']'))
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Some(Key::Ctrl('q'))
        );
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
            Some(Key::Char(']'))
        );
    }

    #[test]
    fn the_sidebar_quits_even_with_the_baud_prompt_open() {
        let mut state = state_with_device();
        state.begin_baud_prompt();
        assert_eq!(map_key(&state, Key::Ctrl('q')), Action::Quit);
        assert_eq!(map_key(&state, Key::F(10)), Action::Quit);
    }

    #[test]
    fn escape_sequence_encoding_is_standard() {
        assert_eq!(encode_key(Key::Esc), Some(b"\x1b".to_vec()));
        assert_eq!(encode_key(Key::Backspace), Some(vec![0x7f]));
        assert_eq!(encode_key(Key::F(1)), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode_key(Key::Ctrl('@')), None);
    }

    #[test]
    fn settings_takes_tab_arrows_space_ctrl_s_and_escape() {
        let mut state = state_with_device();
        state.begin_settings();
        assert_eq!(state.view, View::Settings);
        assert_eq!(map_key(&state, Key::Tab), Action::SettingsMove(1));
        assert_eq!(map_key(&state, Key::BackTab), Action::SettingsMove(-1));
        assert_eq!(map_key(&state, Key::Down), Action::SettingsMove(1));
        assert_eq!(map_key(&state, Key::Up), Action::SettingsMove(-1));
        assert_eq!(map_key(&state, Key::Char(' ')), Action::SettingsToggle);
        assert_eq!(map_key(&state, Key::Enter), Action::SettingsToggle);
        assert_eq!(map_key(&state, Key::Ctrl('s')), Action::SettingsSave);
        assert_eq!(map_key(&state, Key::Esc), Action::SettingsCancel);
        // Nothing typed here reaches a device.
        assert_eq!(map_key(&state, Key::Char('x')), Action::Ignore);
        assert_eq!(map_key(&state, Key::Ctrl(']')), Action::Ignore);
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
