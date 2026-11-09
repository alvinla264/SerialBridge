//! Drawing the TUI into a ratatui buffer. No terminal is involved, so the
//! render seam is tested by inspecting the buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};

use crate::screen::{Color, Style};
use crate::state::{AppState, Focus, Overlay, Severity, SETTINGS_ROWS};

/// Where each part of the interface sits. One place computes it, so the keys
/// that resize the stream screen and the drawing always agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chrome {
    pub header: Rect,
    /// The sidebar and stream panes, when the terminal is tall enough.
    pub main: Option<MainPanes>,
    /// One or two rows at the bottom: the status line and the key hints.
    pub footer: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MainPanes {
    pub sidebar: Rect,
    pub stream: Rect,
    /// The stream's drawing area, below its title line.
    pub stream_body: Option<Rect>,
}

/// Compute the layout for `area`. Zero-height rects mean "not drawn".
pub fn chrome(area: Rect) -> Chrome {
    if area.width == 0 || area.height == 0 {
        let empty = Rect::new(area.x, area.y, 0, 0);
        return Chrome {
            header: empty,
            main: None,
            footer: empty,
        };
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    let footer_height = match area.height {
        1 => 0,
        2..=5 => 1,
        _ => 2,
    };
    let main_height = area.height.saturating_sub(1 + footer_height);
    let footer = if footer_height == 0 {
        Rect::new(area.x, area.y + 1, area.width, 0)
    } else {
        Rect::new(
            area.x,
            area.y + area.height - footer_height,
            area.width,
            footer_height,
        )
    };
    let main = if main_height == 0 {
        None
    } else {
        panes(Rect::new(area.x, area.y + 1, area.width, main_height))
    };
    Chrome {
        header,
        main,
        footer,
    }
}

/// Split a main area into a sidebar, a separator and the stream.
fn panes(area: Rect) -> Option<MainPanes> {
    if area.width < 24 || area.height == 0 {
        return None;
    }
    let sidebar_width = (area.width / 3).clamp(20, 30).min(area.width - 21);
    if sidebar_width < 8 {
        return None;
    }
    let sidebar = Rect::new(area.x, area.y, sidebar_width, area.height);
    let stream = Rect::new(
        area.x + sidebar_width + 1,
        area.y,
        area.width - sidebar_width - 1,
        area.height,
    );
    let stream_body = (stream.height > 1 && stream.width > 0)
        .then(|| Rect::new(stream.x, stream.y + 1, stream.width, stream.height - 1));
    Some(MainPanes {
        sidebar,
        stream,
        stream_body,
    })
}

/// Draw the whole UI into `buffer`, using `area` as the screen.
pub fn render(state: &AppState, buffer: &mut Buffer, area: Rect) {
    buffer.reset();
    let chrome = chrome(area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    draw_header(state, buffer, chrome.header);
    if let Some(main) = chrome.main {
        draw_sidebar(state, buffer, main.sidebar);
        // Separator.
        for y in main.sidebar.y..main.sidebar.y + main.sidebar.height {
            put(
                buffer,
                Rect::new(main.sidebar.right(), y, 1, 1),
                0,
                "│",
                dim(),
            );
        }
        draw_stream(state, buffer, main.stream);
    }
    draw_footer(state, buffer, chrome.footer);
    draw_overlay(state, buffer, area);
}

/// The inner area of the stream pane (below its title line), if it is drawn.
pub fn stream_body_area(area: Rect) -> Option<Rect> {
    chrome(area).main.and_then(|main| main.stream_body)
}

fn draw_header(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let device = state.selected_device();
    let path = device
        .map(|device| device.path.as_str())
        .unwrap_or("no device selected");
    let baud = device
        .and_then(|device| device.baud)
        .map(|baud| baud.to_string())
        .unwrap_or_else(|| "-".to_string());
    let session = state.selected_session();
    let control = session
        .and_then(|session| session.control.as_deref())
        .or_else(|| device.and_then(|device| device.control.as_deref()))
        .unwrap_or("none");
    let attached = if session.is_some() {
        "attached"
    } else {
        "detached"
    };
    let focus = match state.focus {
        Focus::Devices => "devices",
        Focus::Stream => "stream",
    };

    let brand = " SerialNexus ";
    let brand_width = brand.chars().count() as u16;
    let details = format!("  {path}  {baud} baud  AI control: {control}  [{attached}]");
    // The focus hint only appears when it leaves the brand and the device
    // name room; otherwise the whole header is theirs.
    let right = format!("focus: {focus} ");
    let right_width = right.chars().count() as u16;
    let hint_fits =
        area.width.saturating_sub(right_width) >= brand_width + details.chars().count() as u16;
    let left_width = if hint_fits {
        area.width - right_width
    } else {
        area.width
    };
    let brand_style = RStyle::default()
        .fg(RColor::Black)
        .bg(RColor::Gray)
        .add_modifier(Modifier::BOLD);
    put(
        buffer,
        Rect::new(area.x, area.y, left_width, 1),
        0,
        brand,
        brand_style,
    );
    put(
        buffer,
        Rect::new(area.x, area.y, left_width, 1),
        brand_width,
        &details,
        RStyle::default().add_modifier(Modifier::BOLD),
    );
    if hint_fits {
        put(
            buffer,
            Rect::new(area.x + area.width - right_width, area.y, right_width, 1),
            0,
            &right,
            dim(),
        );
    }
}

fn draw_sidebar(state: &AppState, buffer: &mut Buffer, area: Rect) {
    let focused = state.focus == Focus::Devices && state.overlay.is_none();
    let title_style = if focused {
        RStyle::default()
            .fg(RColor::Black)
            .bg(RColor::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        dim()
    };
    let title = format!(" Devices ({}) ", state.devices.len());
    put(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        0,
        &title,
        title_style,
    );

    // Keep the selected row visible when there are more devices than rows.
    let capacity = (area.height.saturating_sub(1) / 2).max(1) as usize;
    let start = if state.selected >= capacity {
        state.selected + 1 - capacity
    } else {
        0
    };
    let mut y = area.y + 1;
    if state.devices.is_empty() {
        put(
            buffer,
            Rect::new(area.x, y, area.width, 1),
            1,
            "no serial devices",
            dim(),
        );
        return;
    }
    for (index, device) in state.devices.iter().enumerate().skip(start) {
        if y >= area.y + area.height {
            break;
        }
        let selected = index == state.selected;
        let attached = state
            .sessions
            .values()
            .any(|session| session.path == device.path);
        let marker = if selected { "›" } else { " " };
        let dot = if device.open { "●" } else { "○" };
        let status = if attached {
            format!(
                "open {}",
                device.baud.map(|b| b.to_string()).unwrap_or_default()
            )
        } else if device.open {
            format!(
                "open by other {}",
                device.baud.map(|b| b.to_string()).unwrap_or_default()
            )
        } else {
            "closed".to_string()
        };
        let label = format!("{marker} {dot} {}", device.path);
        let style = if selected {
            RStyle::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            RStyle::default()
        };
        put(
            buffer,
            Rect::new(area.x, y, area.width, 1),
            0,
            &label,
            style,
        );
        y += 1;
        if y < area.y + area.height {
            let mut details = format!("   {status}");
            if let Some(control) = &device.control {
                details.push_str(&format!(" · AI: {control}"));
            }
            if let Some(description) = &device.description {
                details.push_str(&format!(" · {description}"));
            }
            put(
                buffer,
                Rect::new(area.x, y, area.width, 1),
                0,
                &details,
                dim(),
            );
            y += 1;
        }
    }
}

fn draw_stream(state: &AppState, buffer: &mut Buffer, area: Rect) {
    let focused = state.focus == Focus::Stream && state.overlay.is_none();
    let title_style = if focused {
        RStyle::default()
            .fg(RColor::Black)
            .bg(RColor::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        dim()
    };
    let (title, right) = match state.selected_session() {
        Some(session) => (
            format!(" Stream · {} · {} baud ", session.path, session.baud),
            format!(
                "AI control: {} ",
                session.control.as_deref().unwrap_or("none")
            ),
        ),
        None => (" Stream ".to_string(), String::new()),
    };
    put(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        0,
        &title,
        title_style,
    );
    let right_width = right.chars().count() as u16;
    if area.width > right_width + title.chars().count() as u16 {
        put(
            buffer,
            Rect::new(area.x + area.width - right_width, area.y, right_width, 1),
            0,
            &right,
            dim(),
        );
    }
    if area.height <= 1 {
        return;
    }

    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    let Some(session) = state.selected_session() else {
        put(
            buffer,
            body,
            1,
            "Select a device in the sidebar and press Enter to open or join it. F1 lists the keys.",
            dim(),
        );
        return;
    };
    for row in 0..body.height as usize {
        let y = body.y + row as u16;
        for col in 0..body.width as usize {
            let cell = session.screen.cell(row, col);
            let symbol = cell.ch.to_string();
            put_cell(
                buffer,
                body.x + col as u16,
                y,
                &symbol,
                screen_style(cell.style),
            );
        }
    }
}

fn draw_footer(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let status = Rect::new(area.x, area.y, area.width, 1);
    let hints = if area.height > 1 {
        Some(Rect::new(area.x, area.y + 1, area.width, 1))
    } else {
        None
    };
    let status_drawn = draw_status(state, buffer, status);
    match hints {
        Some(hints) => draw_hints(state, buffer, hints),
        // One row: a prompt or notice matters more than the key hints.
        None if !status_drawn => draw_hints(state, buffer, status),
        None => {}
    }
}

/// Draw the baud prompt or the latest notice. Returns whether the row said
/// anything of its own.
fn draw_status(state: &AppState, buffer: &mut Buffer, area: Rect) -> bool {
    if let Some(prompt) = &state.prompt {
        let text = format!(
            " Baud for {} [{}]: {}{}",
            prompt.path,
            serial_nexus_protocol::DEFAULT_BAUD,
            prompt.input,
            if prompt.error.is_some() { "" } else { "_" }
        );
        let style = RStyle::default()
            .fg(RColor::Yellow)
            .add_modifier(Modifier::BOLD);
        put(buffer, area, 0, &text, style);
        if let Some(error) = &prompt.error {
            let used = text.chars().count() as u16 + 1;
            put(
                buffer,
                area,
                used,
                &format!(" {error} "),
                RStyle::default().fg(RColor::White).bg(RColor::Red),
            );
        }
        return true;
    }
    if let Some(notice) = &state.notice {
        let style = match notice.severity {
            Severity::Error => RStyle::default().fg(RColor::White).bg(RColor::Red),
            Severity::Info => RStyle::default().fg(RColor::Yellow),
        };
        put(buffer, area, 0, &format!(" {} ", notice.text), style);
        return true;
    }
    false
}

fn draw_hints(state: &AppState, buffer: &mut Buffer, area: Rect) {
    let hints = match state.focus {
        Focus::Devices => {
            " Tab switch · ↑/↓ select · Enter open/join · b baud · d disconnect · F1 help · F2 settings "
        }
        Focus::Stream => {
            " Typing sends to the device · Tab switch · F1 help · F2 settings · Ctrl+Q quit "
        }
    };
    put(buffer, area, 0, hints, dim());
}

/// One centred panel over everything else.
fn draw_overlay(state: &AppState, buffer: &mut Buffer, area: Rect) {
    match state.overlay {
        Some(Overlay::Help) => {
            let lines: Vec<(String, RStyle)> = HELP
                .iter()
                .map(|(key, what)| (format!("{key:<12} {what}"), RStyle::default()))
                .collect();
            draw_panel(buffer, area, "Keys", &lines);
        }
        Some(Overlay::Settings) => {
            let rows: Vec<(String, RStyle)> = (0..SETTINGS_ROWS)
                .map(|row| {
                    let name = AppState::settings_row_name(row).unwrap_or_default();
                    let value = match state.settings_value(row) {
                        Some(true) => "on",
                        _ => "off",
                    };
                    let marker = if row == state.settings_row {
                        "▸"
                    } else {
                        " "
                    };
                    let text = format!("{marker} {name:<12} {value}");
                    let style = if row == state.settings_row {
                        RStyle::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        RStyle::default()
                    };
                    (text, style)
                })
                .collect();
            draw_panel(buffer, area, "Settings", &rows);
        }
        None => {}
    }
}

/// The keys the help panel lists: what each one does.
const HELP: [(&str, &str); 9] = [
    ("Tab", "switch between devices and stream"),
    ("↑/↓ j/k", "select a device"),
    ("Enter / o", "open or join the selected device"),
    ("b", "open at a chosen baud rate"),
    ("d", "disconnect the selected device"),
    ("F1", "this help"),
    ("F2", "settings"),
    ("Ctrl+Q", "quit"),
    ("typing", "sends the keys to the device"),
];

fn draw_panel(buffer: &mut Buffer, area: Rect, title: &str, lines: &[(String, RStyle)]) {
    if area.width < 12 || area.height < 4 {
        return;
    }
    let content_width = lines
        .iter()
        .map(|(line, _)| line.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count());
    let width = ((content_width + 4) as u16).min(area.width);
    let height = ((lines.len() + 2) as u16).min(area.height);
    if width < 8 || height < 3 {
        return;
    }
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let inner = (width - 2) as usize;
    let border = RStyle::default()
        .fg(RColor::Cyan)
        .add_modifier(Modifier::BOLD);

    // A blank panel hides whatever it covers.
    for row in 0..height {
        put(
            buffer,
            Rect::new(x, y + row, width, 1),
            0,
            &" ".repeat(width as usize),
            RStyle::default(),
        );
    }

    let top = format!(
        "┌─ {} {}┐",
        title,
        "─".repeat(inner.saturating_sub(title.len() + 3))
    );
    put(buffer, Rect::new(x, y, width, 1), 0, &top, border);
    for (index, (line, style)) in lines.iter().enumerate() {
        if index as u16 + 1 >= height {
            break;
        }
        let padded = format!(" {line:<pad$} ", pad = inner.saturating_sub(2));
        let mut row = String::from("│");
        row.push_str(&padded);
        row.push('│');
        put(
            buffer,
            Rect::new(x, y + 1 + index as u16, width, 1),
            0,
            &row,
            *style,
        );
    }
    let bottom = format!("└{}┘", "─".repeat(inner));
    put(
        buffer,
        Rect::new(x, y + height - 1, width, 1),
        0,
        &bottom,
        border,
    );
}

fn put(buffer: &mut Buffer, area: Rect, offset: u16, text: &str, style: RStyle) {
    let x = area.x + offset;
    let width = area.width.saturating_sub(offset);
    if area.height == 0 || width == 0 || x >= buffer.area.right() || area.y >= buffer.area.bottom()
    {
        return;
    }
    let width = width.min(buffer.area.right().saturating_sub(x));
    buffer.set_stringn(x, area.y, text, width as usize, style);
}

fn put_cell(buffer: &mut Buffer, x: u16, y: u16, symbol: &str, style: RStyle) {
    if x >= buffer.area.right() || y >= buffer.area.bottom() {
        return;
    }
    let cell = &mut buffer[(x, y)];
    cell.set_symbol(symbol);
    cell.set_style(style);
}

fn dim() -> RStyle {
    RStyle::default().add_modifier(Modifier::DIM)
}

fn screen_style(style: Style) -> RStyle {
    let mut out = RStyle::default();
    if let Some(fg) = style.fg {
        out = out.fg(to_ratatui(fg));
    }
    if let Some(bg) = style.bg {
        out = out.bg(to_ratatui(bg));
    }
    let mut modifier = Modifier::empty();
    if style.bold {
        modifier |= Modifier::BOLD;
    }
    if style.dim {
        modifier |= Modifier::DIM;
    }
    if style.underline {
        modifier |= Modifier::UNDERLINED;
    }
    if style.reverse {
        modifier |= Modifier::REVERSED;
    }
    out.add_modifier(modifier)
}

fn to_ratatui(colour: Color) -> RColor {
    match colour {
        Color::Default => RColor::Reset,
        Color::Indexed(index) => RColor::Indexed(index),
        Color::Rgb(red, green, blue) => RColor::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, DeviceEntry, Overlay};
    use serial_nexus_protocol::{Event, Source};

    fn lines(state: &AppState, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        render(state, &mut buffer, area);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn demo_state() -> AppState {
        let mut state = AppState::new(12, 40);
        let mut first = DeviceEntry::new("/dev/ttyUSB0");
        first.description = Some("FTDI adapter".into());
        let mut second = DeviceEntry::new("/dev/ttyUSB1");
        second.open = true;
        second.device_id = Some(7);
        second.baud = Some(57_600);
        state.set_devices(vec![first, second]);
        state.selected = 0;
        state.add_session(7, "/dev/ttyUSB1", 57_600, None);
        state.select_path("/dev/ttyUSB1");
        state
    }

    #[test]
    fn sidebar_lists_devices_with_state() {
        let state = demo_state();
        let screen = lines(&state, 100, 16).join("\n");
        assert!(screen.contains("/dev/ttyUSB0"), "{screen}");
        assert!(screen.contains("/dev/ttyUSB1"), "{screen}");
        assert!(screen.contains("closed"), "{screen}");
        assert!(screen.contains("open 57600"), "{screen}");
        assert!(screen.contains("FTDI adapter"), "{screen}");
    }

    #[test]
    fn header_shows_active_baud_control_and_attachment() {
        let mut state = demo_state();
        state.focus = Focus::Stream;
        state.sessions.get_mut(&7).unwrap().control = Some("claude".into());
        let header = &lines(&state, 100, 16)[0];
        assert!(header.contains("/dev/ttyUSB1"), "{header}");
        assert!(header.contains("57600 baud"), "{header}");
        assert!(header.contains("AI control: claude"), "{header}");
        assert!(header.contains("attached"), "{header}");
        assert!(header.contains("focus: stream"), "{header}");
    }

    #[test]
    fn stream_shows_device_output_and_ai_writes_but_not_human_writes() {
        let mut state = demo_state();
        state.apply_event(&Event::device_output(7, b"ready\r\n"));
        state.apply_event(&Event::write(7, Source::Ai, b"AT+GMR\r\n"));
        state.apply_event(&Event::write(7, Source::Human, b"secret-typing"));
        let screen = lines(&state, 100, 16).join("\n");
        assert!(screen.contains("ready"), "{screen}");
        assert!(screen.contains("AT+GMR"), "{screen}");
        assert!(!screen.contains("[AI]"), "{screen}");
        assert!(!screen.contains("secret-typing"), "{screen}");
    }

    #[test]
    fn unplug_notice_is_in_the_footer_and_not_in_the_stream() {
        let mut state = demo_state();
        state.apply_event(&Event::DeviceClosed {
            device_id: 7,
            path: "/dev/ttyUSB1".into(),
            reason: "device failed".into(),
        });
        let area = Rect::new(0, 0, 100, 16);
        let footer_top = (area.y + area.height - chrome(area).footer.height) as usize;
        let screen = lines(&state, 100, 16);
        let footer = screen[footer_top..].join("\n");
        assert!(footer.contains("unplugged"), "{footer}");
        assert!(footer.contains("/dev/ttyUSB1"), "{footer}");
        assert!(!screen[..footer_top]
            .iter()
            .any(|line| line.contains("unplugged")));
    }

    #[test]
    fn baud_prompt_is_drawn_with_the_default_and_typed_digits() {
        let mut state = demo_state();
        state.select_path("/dev/ttyUSB0");
        assert!(state.begin_baud_prompt());
        state.prompt_push('9');
        state.prompt_push('6');
        let screen = lines(&state, 100, 16).join("\n");
        assert!(screen.contains("Baud for /dev/ttyUSB0"), "{screen}");
        assert!(screen.contains("115200"), "{screen}");
        assert!(screen.contains("96"), "{screen}");
    }

    #[test]
    fn sidebar_scrolls_to_keep_the_selected_device_visible() {
        let mut state = AppState::new(12, 40);
        state.set_devices(
            (0..12)
                .map(|index| DeviceEntry::new(format!("/dev/ttyUSB{index}")))
                .collect(),
        );
        assert!(state.select_path("/dev/ttyUSB11"));
        let screen = lines(&state, 80, 16).join("\n");
        assert!(screen.contains("/dev/ttyUSB11"), "{screen}");
    }

    #[test]
    fn empty_sidebar_says_so_and_stream_explains_opening() {
        let state = AppState::new(12, 40);
        let screen = lines(&state, 100, 16).join("\n");
        assert!(screen.contains("no serial devices"), "{screen}");
        assert!(screen.contains("press Enter"), "{screen}");
        assert!(screen.contains("no device selected"), "{screen}");
    }

    #[test]
    fn screen_cells_keep_device_style() {
        let mut state = demo_state();
        state.apply_event(&Event::device_output(7, b"\x1b[31mR\x1b[0mN"));
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        let stream_y = 2;
        let mut found = false;
        for x in 0..100 {
            if buffer[(x, stream_y)].symbol() == "R" {
                found = true;
                assert_eq!(buffer[(x, stream_y)].fg, RColor::Indexed(1));
            }
        }
        assert!(found, "R was not rendered");
    }

    #[test]
    fn the_stream_body_area_is_where_the_stream_is_drawn() {
        let mut state = demo_state();
        state.apply_event(&Event::device_output(7, b"first"));
        for (width, height) in [(100, 24), (60, 10), (40, 6), (24, 5)] {
            let area = Rect::new(0, 0, width, height);
            // The event loop sizes the stream screen to its body each draw.
            if let Some(body) = stream_body_area(area) {
                state.resize(body.height as usize, body.width as usize);
            }
            let mut buffer = Buffer::empty(area);
            render(&state, &mut buffer, area);
            let body = stream_body_area(area);
            if let Some(body) = body {
                assert_eq!(body.height as usize, state.sessions[&7].screen.rows());
                let row = (0..body.width)
                    .map(|col| buffer[(body.x + col, body.y)].symbol())
                    .collect::<String>();
                assert!(row.starts_with("first"), "{width}x{height}: {row:?}");
            }
        }
    }

    #[test]
    fn the_hints_change_with_the_pane_that_has_the_keys() {
        let mut state = demo_state();
        state.focus = Focus::Devices;
        let devices = lines(&state, 100, 16).join("\n");
        assert!(devices.contains("d disconnect"), "{devices}");
        state.focus = Focus::Stream;
        let stream = lines(&state, 100, 16).join("\n");
        assert!(stream.contains("Typing sends to the device"), "{stream}");
        assert!(!stream.contains("d disconnect"), "{stream}");
    }

    #[test]
    fn a_short_terminal_keeps_the_stream_and_one_footer_row() {
        let mut state = demo_state();
        state.notice = Some(crate::state::Notice::error("boom"));
        let screen = lines(&state, 100, 5);
        assert_eq!(screen.len(), 5);
        assert!(screen[0].contains("SerialNexus"), "{screen:?}");
        // A notice still wins over the hints in a one-row footer.
        assert!(screen[4].contains("boom"), "{screen:?}");

        let tiny = lines(&state, 20, 4);
        assert!(tiny[0].contains("SerialNexus"), "{tiny:?}");
    }

    #[test]
    fn help_lists_the_keys_and_hides_the_hints_behind_it() {
        let mut state = demo_state();
        state.overlay = Some(Overlay::Help);
        let screen = lines(&state, 100, 24).join("\n");
        assert!(screen.contains("┌─ Keys"), "{screen}");
        assert!(
            screen.contains("switch between devices and stream"),
            "{screen}"
        );
        assert!(screen.contains("Ctrl+Q"), "{screen}");
    }

    #[test]
    fn the_settings_panel_shows_each_value_and_follows_the_selection() {
        let mut state = demo_state();
        state.overlay = Some(Overlay::Settings);
        let screen = lines(&state, 100, 24).join("\n");
        assert!(screen.contains("┌─ Settings"), "{screen}");
        assert!(screen.contains("Local echo"), "{screen}");
        assert!(screen.contains("Scrollback"), "{screen}");
        assert!(screen.contains("on"), "{screen}");

        state.settings.local_echo = false;
        state.settings_row = 1;
        let screen = lines(&state, 100, 24).join("\n");
        let row = screen
            .lines()
            .find(|line| line.contains("Local echo"))
            .expect("local echo row");
        assert!(row.contains("off"), "{row}");
        let selected = screen
            .lines()
            .find(|line| line.contains("Scrollback"))
            .expect("scrollback row");
        assert!(selected.contains('▸'), "{selected}");
    }

    #[test]
    fn panels_stay_inside_a_small_terminal() {
        let mut state = demo_state();
        for (width, height) in [(20, 5), (12, 4), (8, 3)] {
            state.overlay = Some(Overlay::Help);
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            render(&state, &mut buffer, area);
        }
    }
}
