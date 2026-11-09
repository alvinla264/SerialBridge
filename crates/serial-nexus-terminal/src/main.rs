//! `serial-nexus-terminal`: a minicom-like human TUI for SerialNexus.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serial_nexus_client::{Client, ClientConfig, ClientInfo};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::Key;
use serial_nexus_terminal::render::{render, stream_body_area};
use serial_nexus_terminal::settings::{SettingsStore, TerminalSettings};
use serial_nexus_terminal::state::Notice;
use serial_nexus_terminal::terminal::{CleanupGuard, CrosstermOps};

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("serial-nexus-terminal: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // The client library's connect seam starts serial-nexus-core when needed.
    let config = ClientConfig::new(ClientInfo::human("serial-nexus-terminal"));
    let client = Client::connect(config)?;

    // Settings are a convenience: an unreadable or invalid file leaves the
    // defaults in effect, is reported, and is never overwritten.
    let (settings, settings_error) = match SettingsStore::from_env() {
        Some(store) => match store.load_or_create() {
            Ok(settings) => (settings, None),
            Err(error) => (
                TerminalSettings::default(),
                Some(format!("settings {}: {error}", store.path().display())),
            ),
        },
        None => (TerminalSettings::default(), None),
    };
    let mut app = App::with_settings(client, 24, 80, settings);
    if let Some(message) = settings_error {
        app.state.notice = Some(Notice::error(message));
    }

    // The guard restores raw mode and the alternate screen on every exit path,
    // including event-loop errors and panics.
    let guard = CleanupGuard::new(CrosstermOps)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = event_loop(&mut terminal, &mut app);

    app.shutdown();
    drop(terminal);
    // Drop the guard before printing an error, so it lands on a sane terminal.
    drop(guard);
    result.map_err(Into::into)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        app.pump();
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some(body) = stream_body_area(area) {
                app.resize_stream(body.height as usize, body.width as usize);
            }
            render(&app.state, frame.buffer_mut(), area);
        })?;
        if app.state.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                event::Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(key) = translate(key) {
                        app.handle_key(key);
                    }
                }
                event::Event::Resize(_, _) => terminal.autoresize()?,
                _ => {}
            }
        }
        if last_refresh.elapsed() >= Duration::from_secs(2) {
            app.refresh_devices();
            last_refresh = Instant::now();
        }
    }
}

fn translate(event: KeyEvent) -> Option<Key> {
    let control = event.modifiers.contains(KeyModifiers::CONTROL);
    match event.code {
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
