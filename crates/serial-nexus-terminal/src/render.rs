//! Drawing the TUI into a ratatui buffer. No terminal is involved, so the
//! render seam is tested by inspecting the buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    if area.width < 20 || area.height == 0 {
        return None;
    }
    // The sidebar gives way first: the stream keeps at least 12 columns.
    let sidebar_width = (area.width / 3).clamp(14, 30).min(area.width - 13);
    if sidebar_width < 6 {
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
    let brand_width = display_width(brand);
    let details = format!("  {path}  {baud} baud  AI control: {control}  [{attached}]");
    // The focus hint only appears when it leaves the brand and the device
    // name room; otherwise the whole header is theirs.
    let right = format!("focus: {focus} ");
    let right_width = display_width(&right);
    let hint_fits = area.width.saturating_sub(right_width) >= brand_width + display_width(&details);
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
    let right_width = display_width(&right);
    if area.width > right_width + display_width(&title) {
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
            let lines: Vec<(String, RStyle, bool)> = HELP
                .iter()
                .map(|(key, what)| (format!("{key:<12} {what}"), RStyle::default(), false))
                .collect();
            draw_panel(buffer, area, "Keys", &lines, state.help_offset);
        }
        Some(Overlay::Settings) => {
            let mut rows: Vec<(String, RStyle, bool)> = (0..SETTINGS_ROWS)
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
                    let selected = row == state.settings_row;
                    let style = if selected {
                        RStyle::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        RStyle::default()
                    };
                    (text, style, selected)
                })
                .collect();
            rows.push(("Esc closes · Space toggles".to_string(), dim(), false));
            let selected = rows
                .iter()
                .position(|(_, _, selected)| *selected)
                .unwrap_or(0);
            draw_panel(buffer, area, "Settings", &rows, selected);
        }
        None => {}
    }
}

/// The keys the help panel lists: what each one does.
const HELP: [(&str, &str); 10] = [
    ("Esc", "close this panel"),
    ("Tab", "switch between devices and stream"),
    ("↑/↓ j/k", "select a device, or a setting"),
    ("Enter / o", "open or join the selected device"),
    ("b", "open at a chosen baud rate"),
    ("d", "disconnect the selected device"),
    ("Space", "toggle the selected setting"),
    ("F1", "this help"),
    ("F2", "settings"),
    ("Ctrl+Q", "quit"),
];

fn draw_panel(
    buffer: &mut Buffer,
    area: Rect,
    title: &str,
    lines: &[(String, RStyle, bool)],
    keep_in_view: usize,
) {
    // Too small for a box: one line still says the panel is open, so its keys
    // are never swallowed silently.
    if area.width < 12 || area.height < 4 {
        // The way out comes first, so it survives a narrow terminal.
        let text = fit(&format!(" Esc closes · {title} "), area.width as usize);
        put(
            buffer,
            area,
            0,
            &text,
            RStyle::default()
                .fg(RColor::Black)
                .bg(RColor::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        return;
    }
    let content_width = lines
        .iter()
        .map(|(line, _, _)| display_width(line) as usize)
        .max()
        .unwrap_or(0)
        .max(display_width(title) as usize);
    let width = ((content_width + 4) as u16).min(area.width);
    // Room for both borders and at least two content rows, or the one-line
    // fallback above says the panel is open.
    let height = ((lines.len() + 2) as u16).max(4).min(area.height);
    if width < 8 || height < 4 {
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

    // The title gives way to a narrow box instead of pushing the border off.
    let title = clip(title, inner.saturating_sub(4));
    let top = format!(
        "┌─ {} {}┐",
        title,
        "─".repeat(inner.saturating_sub(display_width(&title) as usize + 3))
    );
    put(buffer, Rect::new(x, y, width, 1), 0, &top, border);

    // When the box is shorter than the list, scroll it: the selected row (or
    // the help list's own scroll position) stays in view.
    let visible = (height - 2) as usize;
    let last_first = lines.len().saturating_sub(visible);
    let first = keep_in_view
        .saturating_sub(visible.saturating_sub(1))
        .min(last_first);
    for (offset, (line, style, _)) in lines.iter().skip(first).enumerate() {
        if offset >= visible {
            break;
        }
        let mut row = String::from("│");
        row.push_str(&fit(&format!(" {line}"), inner));
        row.push('│');
        put(
            buffer,
            Rect::new(x, y + 1 + offset as u16, width, 1),
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

/// One line as exactly `width` display columns: clipped if longer, padded if
/// shorter. Wide characters are never split down the middle.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > width {
            break;
        }
        out.push(ch);
        used += cell;
    }
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}

/// The first `width` display columns of `text`.
fn clip(text: &str, width: usize) -> String {
    fit(text, width).trim_end().to_string()
}

fn display_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
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
    fn the_settings_panel_shows_the_value_it_saves() {
        let mut state = demo_state();
        state.overlay = Some(Overlay::Settings);
        let screen = lines(&state, 100, 24).join("\n");
        assert!(screen.contains("┌─ Settings"), "{screen}");
        let row = screen
            .lines()
            .find(|line| line.contains("Local echo"))
            .expect("local echo row");
        assert!(row.contains("off"), "{row}");
        assert!(row.contains('▸'), "{row}");
        assert!(
            !screen.contains("Scrollback"),
            "a control that does nothing is not offered: {screen}"
        );

        state.settings.local_echo = true;
        let screen = lines(&state, 100, 24).join("\n");
        let row = screen
            .lines()
            .find(|line| line.contains("Local echo"))
            .expect("local echo row");
        assert!(row.contains("on"), "{row}");
    }

    #[test]
    fn panels_stay_inside_a_small_terminal_and_stay_visible() {
        let mut state = demo_state();
        // A box that fits keeps its borders and its content within them.
        state.overlay = Some(Overlay::Settings);
        let screen = lines(&state, 20, 8);
        let top = screen
            .iter()
            .position(|line| line.contains("┌─ Settings"))
            .expect("panel top");
        let bottom = screen
            .iter()
            .position(|line| line.contains("└"))
            .expect("panel bottom");
        assert!(bottom > top, "{screen:?}");
        for line in &screen[top..=bottom] {
            let width = UnicodeWidthStr::width(line.as_str());
            let left = line
                .chars()
                .position(|ch| ch == '│' || ch == '┌' || ch == '└');
            assert!(left.is_some(), "{line:?}");
            assert!(width <= 20, "{line:?}");
            assert!(
                line.ends_with('│') || line.ends_with('┐') || line.ends_with('┘'),
                "a clipped panel keeps both borders: {line:?}"
            );
        }

        // Too small for a box: one line still says the panel is open.
        for (width, height) in [(80, 3), (12, 2), (8, 3)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            render(&state, &mut buffer, area);
            let screen = lines(&state, width, height).join("\n");
            assert!(screen.contains("Esc"), "{width}x{height}: {screen}");
        }
    }

    #[test]
    fn the_hint_and_the_panel_account_for_wide_characters() {
        let mut state = demo_state();
        state.sessions.get_mut(&7).unwrap().control = Some("界界界界界界".into());
        let screen = lines(&state, 130, 24);
        let title = screen
            .iter()
            .find(|line| line.contains("Stream ·"))
            .expect("stream title");
        assert!(
            title.contains("AI control:") && title.matches('界').count() == 6,
            "the whole holder fits: {title:?}"
        );

        // …and the focus hint is dropped when it would cover a wide device
        // path rather than clipped into it.
        let mut state = AppState::new(10, 60);
        state.set_devices(vec![DeviceEntry::new(
            "/dev/界界界界界界界界界界界界界界界界界界界界",
        )]);
        let header = &lines(&state, 60, 10)[0];
        assert!(
            !header.contains("focus:"),
            "the hint does not cover the device: {header:?}"
        );
        assert!(
            header.contains("界面") || header.contains('界'),
            "{header:?}"
        );
    }
}
