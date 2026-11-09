//! Drawing the TUI into a ratatui buffer. No terminal is involved, so the
//! render seam is tested by inspecting the buffer.
//!
//! One layout function decides where everything sits, including the cells the
//! mouse can click, so drawing, resizing the stream screen and hit testing
//! always agree.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::screen::{Color, Style};
use crate::state::{AppState, Severity, View, SETTINGS_ROWS};

/// Where each part of the interface sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chrome {
    pub header: Rect,
    /// The area the stream view, the sidebar or the settings use.
    pub main: Rect,
    /// One or two rows at the bottom: the status line and the key hints.
    pub footer: Rect,
}

/// Compute the layout for `area`. Zero-height rects mean "not drawn".
pub fn chrome(area: Rect) -> Chrome {
    if area.width == 0 || area.height == 0 {
        let empty = Rect::new(area.x, area.y, 0, 0);
        return Chrome {
            header: empty,
            main: empty,
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
    let main = Rect::new(area.x, area.y + 1, area.width, main_height);
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
    Chrome {
        header,
        main,
        footer,
    }
}

/// The stream's drawing area. The sidebar hides while the stream is shown, so
/// the stream gets the whole main area; sessions are sized to it even when
/// another view is on screen.
pub fn stream_body_area(area: Rect) -> Option<Rect> {
    let main = chrome(area).main;
    (main.width > 0 && main.height > 0).then_some(main)
}

/// The sidebar's rows: a title, the device list, and the Settings button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sidebar {
    pub title: Rect,
    pub list: Rect,
    pub button: Rect,
}

/// Where the sidebar puts its parts, or `None` when there is no room.
pub fn sidebar(area: Rect) -> Option<Sidebar> {
    let main = chrome(area).main;
    if main.height < 2 || main.width < 8 {
        return None;
    }
    let title = Rect::new(main.x, main.y, main.width, 1);
    let button = Rect::new(main.x, main.y + main.height - 1, main.width, 1);
    let list_height = main.height - 2;
    let list = Rect::new(main.x, main.y + 1, main.width, list_height);
    Some(Sidebar {
        title,
        list,
        button,
    })
}

/// The Settings button's row, for the mouse and for tests.
pub fn settings_button(area: Rect) -> Option<Rect> {
    sidebar(area).map(|sidebar| sidebar.button)
}

/// The settings rows: a title, the rows, and (when there is room) the key
/// hints. A short terminal drops the hints rather than a flag: both flags stay
/// visible down to a three-row area.
pub fn settings_rows(area: Rect) -> Option<(Rect, Rect, Rect)> {
    let main = chrome(area).main;
    if main.height < 2 || main.width < 8 {
        return None;
    }
    let hint_height = if main.height >= 4 { 1 } else { 0 };
    let title = Rect::new(main.x, main.y, main.width, 1);
    let hints = Rect::new(
        main.x,
        main.y + main.height - hint_height,
        main.width,
        hint_height,
    );
    let rows = Rect::new(
        main.x,
        main.y + 1,
        main.width,
        main.height - 1 - hint_height,
    );
    Some((title, rows, hints))
}

/// What a click in the sidebar landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Device(usize),
    Settings,
}

/// Hit test one click against the sidebar as it is drawn.
pub fn hit_test(state: &AppState, area: Rect, column: u16, row: u16) -> Option<Hit> {
    let sidebar = sidebar(area)?;
    if in_rect(sidebar.button, column, row) {
        return Some(Hit::Settings);
    }
    let list = sidebar.list;
    if list.height == 0 || !in_rect(list, column, row) {
        return None;
    }
    let index = (row - list.y) as usize + first_device_row(state, sidebar);
    (index < state.devices.len()).then_some(Hit::Device(index))
}

/// The first device index the list shows, so the selection stays in view.
fn first_device_row(state: &AppState, sidebar: Sidebar) -> usize {
    let capacity = sidebar.list.height.max(1) as usize;
    if state.selected >= capacity {
        state.selected + 1 - capacity
    } else {
        0
    }
}

fn in_rect(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Draw the whole UI into `buffer`, using `area` as the screen, and return
/// where the terminal cursor goes.
pub fn render(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    buffer.reset();
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let chrome = chrome(area);
    draw_header(state, buffer, chrome.header);
    let cursor = match state.view {
        View::Stream => draw_stream(state, buffer, chrome.main),
        View::Devices => draw_sidebar(state, buffer, area),
        View::Settings => draw_settings(state, buffer, area),
    };
    draw_footer(state, buffer, chrome.footer);
    cursor
}

fn draw_header(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let device = state.shown_device();
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
    let view = match state.view {
        View::Devices => "sidebar",
        View::Stream => "stream",
        View::Settings => "settings",
    };

    let brand = " SerialNexus ";
    let brand_width = display_width(brand);
    let details = format!("  {path}  {baud} baud  AI control: {control}  [{attached}]");
    let right = format!("{view} ");
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

/// The sidebar: every device, then the Settings button, then room for more
/// devices than fit.
fn draw_sidebar(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let sidebar = sidebar(area)?;
    let list = sidebar.list;
    // With no room for a device row, the title row carries the selected device
    // instead, so the cursor never points at the Settings button while a
    // device owns the keys.
    let title = if list.height == 0 && !state.settings_button_selected() {
        match state.selected_device() {
            Some(device) => format!("› {} ", device.path),
            None => " Devices ".to_string(),
        }
    } else {
        format!(" Devices ({}) ", state.devices.len())
    };
    let title_style = RStyle::default()
        .fg(RColor::Black)
        .bg(RColor::Cyan)
        .add_modifier(Modifier::BOLD);
    put(buffer, sidebar.title, 0, &title, title_style);
    if state.devices.is_empty() && list.height > 0 {
        put(
            buffer,
            list,
            1,
            "no serial devices — is the core running?",
            dim(),
        );
    }
    let first = first_device_row(state, sidebar);
    for (offset, (index, device)) in state.devices.iter().enumerate().skip(first).enumerate() {
        if offset >= list.height as usize {
            break;
        }
        let y = list.y + offset as u16;
        let row = Rect::new(list.x, y, list.width, 1);
        let selected = index == state.selected;
        let marker = if selected { "›" } else { " " };
        let dot = if device.open { "●" } else { "○" };
        let label = format!("{marker} {dot} {}", device.path);
        let attached = state
            .sessions
            .values()
            .any(|session| session.path == device.path);
        let mut status = if attached {
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
        if let Some(control) = &device.control {
            status.push_str(&format!(" · AI: {control}"));
        }
        if let Some(description) = &device.description {
            status.push_str(&format!(" · {description}"));
        }
        draw_row(buffer, row, &label, &status, highlighted(selected));
    }

    // The Settings button is a real row: selected with the arrow keys and
    // clickable with the mouse.
    let selected = state.settings_button_selected();
    let button = sidebar.button;
    let style = if selected {
        highlighted(true)
    } else {
        RStyle::default().fg(RColor::Cyan)
    };
    draw_row(
        buffer,
        button,
        &format!("{} [ Settings ]", if selected { "›" } else { " " }),
        "Enter opens",
        style,
    );

    if state.settings_button_selected() {
        Some((button.x + 1, button.y))
    } else if list.height == 0 {
        // The device is shown on the title row when no device row fits.
        Some((sidebar.title.x + 1, sidebar.title.y))
    } else {
        // The selected device row, which the list has kept in view.
        let offset = state.selected.saturating_sub(first) as u16;
        Some((list.x + 1, list.y + offset))
    }
}

/// The settings view in the terminal area: the two flags, edited as a draft.
fn draw_settings(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    let (title, rows, hints) = settings_rows(area)?;
    put(
        buffer,
        title,
        0,
        " Settings ",
        RStyle::default()
            .fg(RColor::Black)
            .bg(RColor::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    // Scroll only if the flags cannot all fit; the selected one stays visible.
    let visible = rows.height.max(1) as usize;
    let last_first = SETTINGS_ROWS.saturating_sub(visible);
    let first = state
        .settings_row
        .min(SETTINGS_ROWS - 1)
        .saturating_sub(visible.saturating_sub(1))
        .min(last_first);
    for row in first..SETTINGS_ROWS {
        if row - first >= visible {
            break;
        }
        let name = AppState::settings_row_name(row).unwrap_or_default();
        let value = match state.draft_value(row) {
            Some(true) => "on",
            _ => "off",
        };
        let selected = row == state.settings_row;
        let line = Rect::new(rows.x, rows.y + (row - first) as u16, rows.width, 1);
        draw_row(
            buffer,
            line,
            &format!("{} {name}", if selected { "▸" } else { " " }),
            value,
            highlighted(selected),
        );
    }
    put(
        buffer,
        hints,
        0,
        " Local echo and scrollback · Ctrl+S save · Esc cancel ",
        dim(),
    );

    let row = state.settings_row.min(SETTINGS_ROWS - 1);
    let value = match state.draft_value(row) {
        Some(true) => "on",
        _ => "off",
    };
    // The cursor sits where the value is drawn, right-aligned at the edge, on
    // the row the list actually scrolled to.
    let value_column = rows.width.saturating_sub(display_width(value));
    Some((rows.x + value_column, rows.y + (row - first) as u16))
}

fn draw_stream(state: &AppState, buffer: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let Some(session) = state.selected_session() else {
        put(
            buffer,
            area,
            1,
            "Select a device in the sidebar and press Enter to open or join it.",
            dim(),
        );
        return Some((area.x + 1, area.y));
    };
    for row in 0..area.height as usize {
        let y = area.y + row as u16;
        for col in 0..area.width as usize {
            let cell = session.screen.cell(row, col);
            let symbol = cell.ch.to_string();
            put_cell(
                buffer,
                area.x + col as u16,
                y,
                &symbol,
                screen_style(cell.style),
            );
        }
    }
    // The device's own cursor, so typing happens where the device expects it.
    let (row, col) = session.screen.cursor();
    Some((
        area.x + col.min(area.width.saturating_sub(1) as usize) as u16,
        area.y + row.min(area.height.saturating_sub(1) as usize) as u16,
    ))
}

fn draw_footer(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.height == 0 {
        return;
    }
    let status = Rect::new(area.x, area.y, area.width, 1);
    let hints = (area.height > 1).then(|| Rect::new(area.x, area.y + 1, area.width, 1));
    let status_drawn = draw_status(state, buffer, status);
    match hints {
        Some(hints) => draw_hints(state, buffer, hints),
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
    let hints = match state.view {
        View::Devices => {
            " ↑/↓ select · Enter open/join · b baud · d disconnect · Ctrl+] stream · Ctrl+Q quit "
        }
        View::Stream => {
            " Typing goes to the device (Tab and Ctrl+Q too) · Ctrl+] back to the sidebar "
        }
        View::Settings => " Ctrl+S save · Esc cancel · Tab/↑↓ move · Space toggle ",
    };
    put(buffer, area, 0, hints, dim());
}

/// One sidebar or settings row: a label on the left, a value kept at the
/// right edge, and the selection style.
fn draw_row(buffer: &mut Buffer, area: Rect, label: &str, value: &str, style: RStyle) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text = row_text(label, value, area.width as usize);
    put(buffer, area, 0, &text, style);
}

fn row_text(label: &str, value: &str, width: usize) -> String {
    if value.is_empty() {
        return fit(label, width);
    }
    let label_width = display_width(label) as usize;
    let value_width = display_width(value) as usize;
    if label_width + value_width < width {
        let mut line = label.to_string();
        line.push_str(&" ".repeat(width - label_width - value_width));
        line.push_str(value);
        fit(&line, width)
    } else {
        // Too narrow for both: keep the value, clip the label.
        let keep = width.saturating_sub(value_width + 1);
        let mut line = fit(&clip(label, keep), keep);
        line.push(' ');
        line.push_str(value);
        fit(&line, width)
    }
}

fn highlighted(selected: bool) -> RStyle {
    if selected {
        RStyle::default()
            .fg(RColor::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        RStyle::default()
    }
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
    use crate::state::{AppState, DeviceEntry, View};
    use serial_nexus_protocol::{Event, Source};

    fn screen(state: &AppState, width: u16, height: u16) -> Vec<String> {
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

    fn demo() -> AppState {
        let mut state = AppState::new(12, 40);
        let mut first = DeviceEntry::new("/dev/ttyUSB0");
        first.description = Some("FTDI adapter".into());
        let mut second = DeviceEntry::new("/dev/ttyUSB1");
        second.open = true;
        second.device_id = Some(7);
        second.baud = Some(57_600);
        state.set_devices(vec![first, second]);
        state.add_session(7, "/dev/ttyUSB1", 57_600, None);
        state.select_path("/dev/ttyUSB1");
        state
    }

    #[test]
    fn the_sidebar_lists_devices_with_state_and_a_settings_button() {
        let state = demo();
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Devices (2)"), "{text}");
        assert!(text.contains("/dev/ttyUSB0"), "{text}");
        assert!(text.contains("/dev/ttyUSB1"), "{text}");
        assert!(text.contains("closed"), "{text}");
        assert!(text.contains("open 57600"), "{text}");
        assert!(text.contains("FTDI adapter"), "{text}");
        assert!(text.contains("[ Settings ]"), "{text}");
    }

    #[test]
    fn the_stream_hides_the_sidebar_and_uses_the_whole_width() {
        let mut state = demo();
        state.view = View::Stream;
        state.apply_event(&Event::device_output(7, b"ready\r\n"));
        state.apply_event(&Event::write(7, Source::Ai, b"AT+GMR\r\n"));
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        let text = screen(&state, 100, 16).join("\n");
        assert!(
            !text.contains("Devices ("),
            "no sidebar in the stream: {text}"
        );
        assert!(text.contains("ready"), "{text}");
        assert!(text.contains("AT+GMR"), "{text}");
        // The stream starts at the left edge, not after a sidebar.
        assert_eq!(buffer[(0, 1)].symbol(), "r");
        assert_eq!(chrome(area).main.height, 13, "header and footer only");
    }

    #[test]
    fn the_header_names_the_device_and_the_view() {
        let mut state = demo();
        state.sessions.get_mut(&7).unwrap().control = Some("claude".into());
        let header = &screen(&state, 100, 16)[0];
        assert!(header.contains("/dev/ttyUSB1"), "{header}");
        assert!(header.contains("57600 baud"), "{header}");
        assert!(header.contains("AI control: claude"), "{header}");
        assert!(header.contains("attached"), "{header}");
        assert!(header.contains("sidebar"), "{header}");

        state.view = View::Stream;
        assert!(screen(&state, 100, 16)[0].contains("stream"));
    }

    #[test]
    fn the_cursor_sits_on_the_device_cursor_in_the_stream() {
        let mut state = demo();
        state.view = View::Stream;
        state.apply_event(&Event::device_output(7, b"boot> "));
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area);
        assert_eq!(cursor, Some((6, 1)), "six columns along the first row");
    }

    #[test]
    fn the_cursor_sits_on_the_selected_sidebar_row_and_the_button() {
        let mut state = demo();
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let list = sidebar(area).unwrap().list;
        let first = first_device_row(&state, sidebar(area).unwrap());
        assert_eq!(
            cursor,
            (list.x + 1, list.y + (state.selected - first) as u16)
        );

        state.select_settings_button();
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let button = settings_button(area).expect("button");
        assert_eq!(cursor, (button.x + 1, button.y));
    }

    #[test]
    fn clicks_find_device_rows_and_the_settings_button() {
        let state = demo();
        let area = Rect::new(0, 0, 100, 16);
        let sidebar = sidebar(area).unwrap();
        assert_eq!(
            hit_test(&state, area, sidebar.list.x, sidebar.list.y),
            Some(Hit::Device(0))
        );
        let offset = (state.devices.len() - 1) as u16;
        assert_eq!(
            hit_test(&state, area, sidebar.list.x, sidebar.list.y + offset),
            Some(Hit::Device(1))
        );
        assert_eq!(
            hit_test(&state, area, sidebar.button.x, sidebar.button.y),
            Some(Hit::Settings)
        );
        assert_eq!(
            hit_test(&state, area, 0, 0),
            None,
            "the header is not a control"
        );
    }

    #[test]
    fn the_settings_view_shows_both_flags_and_the_cursor_follows_the_selection() {
        let mut state = demo();
        state.begin_settings();
        state.draft.local_echo = false;
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("Local echo"), "{text}");
        assert!(text.contains("Scrollback"), "{text}");
        let row = text
            .lines()
            .find(|line| line.contains("Local echo"))
            .expect("local echo row");
        assert!(row.contains("off"), "{row}");
        let scrollback = text
            .lines()
            .find(|line| line.contains("Scrollback"))
            .expect("scrollback row");
        assert!(scrollback.contains("on"), "{scrollback}");
        assert!(text.contains("Ctrl+S save"), "{text}");

        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let (_, rows, _) = settings_rows(area).unwrap();
        assert_eq!(cursor.1, rows.y);

        state.settings_row = 1;
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        assert_eq!(cursor.1, rows.y + 1);
    }

    #[test]
    fn a_short_settings_view_still_shows_both_flags() {
        let mut state = demo();
        state.begin_settings();
        state.settings_row = 1;
        let lines = screen(&state, 80, 6);
        let text = lines.join("\n");
        assert!(text.contains("Local echo"), "{text}");
        assert!(text.contains("Scrollback"), "{text}");
        let area = Rect::new(0, 0, 80, 6);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let (_, rows, _) = settings_rows(area).unwrap();
        assert!(
            cursor.1 >= rows.y && cursor.1 < rows.y + rows.height,
            "the cursor stays on a flag row, not the hints: {cursor:?}"
        );
        assert!(
            lines[cursor.1 as usize].contains("Scrollback"),
            "the cursor row is the selected flag: {:?}",
            lines[cursor.1 as usize]
        );
    }

    #[test]
    fn a_four_row_sidebar_shows_the_selected_device_under_the_cursor() {
        let state = demo();
        let area = Rect::new(0, 0, 80, 4);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let row: String = (0..area.width)
            .map(|x| buffer[(x, cursor.1)].symbol())
            .collect();
        assert!(
            row.contains(&state.devices[state.selected].path),
            "the cursor row names the selected device: {row:?}"
        );
        assert!(!row.contains("[ Settings ]"), "{row:?}");
    }

    #[test]
    fn a_four_row_settings_view_keeps_the_cursor_on_the_selected_flag() {
        let mut state = demo();
        state.begin_settings();
        state.settings_row = 1;
        let area = Rect::new(0, 0, 80, 4);
        let mut buffer = Buffer::empty(area);
        let cursor = render(&state, &mut buffer, area).expect("cursor");
        let (_, rows, _) = settings_rows(area).unwrap();
        assert!(
            cursor.1 >= rows.y && cursor.1 < rows.y + rows.height,
            "the cursor is on a flag row, not the footer: {cursor:?}"
        );
        let row_text: String = (0..area.width)
            .map(|x| buffer[(x, cursor.1)].symbol())
            .collect();
        assert!(row_text.contains("Scrollback"), "{row_text:?}");
    }

    #[test]
    fn the_footer_hints_match_the_view() {
        let mut state = demo();
        let sidebar_hints = screen(&state, 100, 16).join("\n");
        assert!(sidebar_hints.contains("Ctrl+Q quit"), "{sidebar_hints}");
        state.view = View::Stream;
        let stream_hints = screen(&state, 100, 16).join("\n");
        assert!(stream_hints.contains("Ctrl+] back"), "{stream_hints}");
        assert!(!stream_hints.contains("Ctrl+Q quit"), "{stream_hints}");
        state.begin_settings();
        let settings_hints = screen(&state, 100, 16).join("\n");
        assert!(settings_hints.contains("Esc cancel"), "{settings_hints}");
    }

    #[test]
    fn a_notice_leads_the_footer_and_stays_out_of_the_stream() {
        let mut state = demo();
        state.view = View::Stream;
        state.notice = Some(crate::state::Notice::error("write failed"));
        let area = Rect::new(0, 0, 100, 16);
        let footer_top = (area.y + area.height - chrome(area).footer.height) as usize;
        let lines = screen(&state, 100, 16);
        let footer = lines[footer_top..].join("\n");
        assert!(footer.contains("write failed"), "{footer}");
        assert!(!lines[..footer_top]
            .iter()
            .any(|line| line.contains("write failed")));
    }

    #[test]
    fn the_baud_prompt_is_drawn_with_the_default_and_typed_digits() {
        let mut state = demo();
        state.select_path("/dev/ttyUSB0");
        assert!(state.begin_baud_prompt());
        state.prompt_push('9');
        state.prompt_push('6');
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("Baud for /dev/ttyUSB0"), "{text}");
        assert!(text.contains("115200"), "{text}");
        assert!(text.contains("96"), "{text}");
    }

    #[test]
    fn the_sidebar_scrolls_to_keep_the_selection_visible() {
        let mut state = AppState::new(12, 40);
        state.set_devices(
            (0..12)
                .map(|index| DeviceEntry::new(format!("/dev/ttyUSB{index}")))
                .collect(),
        );
        assert!(state.select_path("/dev/ttyUSB11"));
        let text = screen(&state, 80, 16).join("\n");
        assert!(text.contains("/dev/ttyUSB11"), "{text}");
        // The Settings button is still there at the bottom.
        assert!(text.contains("[ Settings ]"), "{text}");
    }

    #[test]
    fn a_short_terminal_keeps_the_stream_and_one_footer_row() {
        let mut state = demo();
        state.view = View::Stream;
        state.notice = Some(crate::state::Notice::error("boom"));
        let lines = screen(&state, 100, 5);
        assert!(lines[0].contains("SerialNexus"), "{lines:?}");
        assert!(lines[4].contains("boom"), "{lines:?}");

        let tiny = screen(&state, 20, 4);
        assert!(tiny[0].contains("SerialNexus"), "{tiny:?}");
    }

    #[test]
    fn the_sidebar_says_so_when_there_are_no_devices() {
        let state = AppState::new(12, 40);
        let text = screen(&state, 100, 16).join("\n");
        assert!(text.contains("no serial devices"), "{text}");
        assert!(text.contains("no device selected"), "{text}");
        assert!(text.contains("[ Settings ]"), "{text}");
    }

    #[test]
    fn screen_cells_keep_the_device_style() {
        let mut state = demo();
        state.view = View::Stream;
        state.apply_event(&Event::device_output(7, b"\x1b[31mR\x1b[0mN"));
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        render(&state, &mut buffer, area);
        assert_eq!(buffer[(0, 1)].symbol(), "R");
        assert_eq!(buffer[(0, 1)].fg, RColor::Indexed(1));
    }

    #[test]
    fn wide_characters_are_measured_in_columns() {
        let mut state = demo();
        state.set_devices(vec![DeviceEntry::new(
            "/dev/界界界界界界界界界界界界界界界界界界界界",
        )]);
        let header = &screen(&state, 60, 10)[0];
        assert!(
            !header.contains("sidebar"),
            "the view hint gives way: {header:?}"
        );

        let mut state = demo();
        state.sessions.get_mut(&7).unwrap().control = Some("界界界界界界".into());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 16));
        render(&state, &mut buffer, Rect::new(0, 0, 120, 16));
        let header = &screen(&state, 120, 16)[0];
        assert_eq!(header.matches('界').count(), 6, "{header:?}");
    }
}
