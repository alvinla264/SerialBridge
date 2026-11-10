//! `serial-nexus-terminal`: a minicom-like human TUI for SerialNexus.

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, KeyEventKind, MouseEvent};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use serial_nexus_client::{Client, ClientConfig, ClientInfo};
use serial_nexus_terminal::app::App;
use serial_nexus_terminal::input::translate;
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
    let (settings, store, settings_error) = match SettingsStore::from_env() {
        Some(store) => match store.load_or_create() {
            Ok(settings) => (settings, Some(store), None),
            Err(error) => (
                TerminalSettings::default(),
                // Changes are not saved over a file the human hand-edited into
                // an invalid state: they apply to this launch only.
                None,
                Some(format!("settings {}: {error}", store.path().display())),
            ),
        },
        None => (TerminalSettings::default(), None, None),
    };
    let mut app = App::with_preferences(client, 24, 80, settings, store);
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
    let mut last_area = Rect::default();
    loop {
        app.pump();
        terminal.draw(|frame| {
            let area = frame.area();
            last_area = area;
            if let Some(body) = stream_body_area(area) {
                app.resize_stream(body.height as usize, body.width as usize);
            }
            if let Some((x, y)) = render(&app.state, frame.buffer_mut(), area) {
                frame.set_cursor_position((x, y));
            }
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
                event::Event::Mouse(mouse) => handle_mouse(app, last_area, mouse),
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

/// A click on a device row highlights it; a double-click opens or joins it,
/// just as Enter does. A click on the sidebar's Settings button opens the
/// settings. In the scrollback view, the wheel scrolls the kept output.
fn handle_mouse(app: &mut App, area: Rect, mouse: MouseEvent) {
    app.handle_mouse(area, mouse.kind, mouse.column, mouse.row, Instant::now());
}
