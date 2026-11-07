//! Drawing the TUI into a ratatui buffer. No terminal is involved, so the
//! render seam is tested by inspecting the buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};

use crate::screen::{Color, Style};
use crate::state::{AppState, Focus, Severity};

/// Draw the whole UI into `buffer`, using `area` as the screen.
pub fn render(state: &AppState, buffer: &mut Buffer, area: Rect) {
    buffer.reset();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let header = Rect::new(area.x, area.y, area.width, 1);
    draw_header(state, buffer, header);

    if area.height >= 3 {
        let footer = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        let main = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
        draw_main(state, buffer, main);
        draw_footer(state, buffer, footer);
    }
}

fn draw_header(state: &AppState, buffer: &mut Buffer, area: Rect) {
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
    let text = format!(
        " SerialNexus  {path}  {baud} baud  AI control: {control}  [{attached}]  focus: {focus}"
    );
    let style = RStyle::default()
        .fg(RColor::Black)
        .bg(RColor::Gray)
        .add_modifier(Modifier::BOLD);
    put(buffer, area, 0, &text, style);
}

fn draw_main(state: &AppState, buffer: &mut Buffer, area: Rect) {
    if area.width < 20 || area.height == 0 {
        return;
    }
    let sidebar_width = (area.width / 3).clamp(18, 32).min(area.width - 12);
    let sidebar = Rect::new(area.x, area.y, sidebar_width, area.height);
    let stream = Rect::new(
        area.x + sidebar_width + 1,
        area.y,
        area.width - sidebar_width - 1,
        area.height,
    );

    draw_sidebar(state, buffer, sidebar);
    // Separator.
    for y in area.y..area.y + area.height {
        put(
            buffer,
            Rect::new(area.x + sidebar_width, y, 1, 1),
            0,
            "│",
            dim(),
        );
    }
    draw_stream(state, buffer, stream);
}

fn draw_sidebar(state: &AppState, buffer: &mut Buffer, area: Rect) {
    let focused = state.focus == Focus::Devices;
    let title_style = if focused {
        RStyle::default()
            .fg(RColor::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
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
        let marker = if selected {
            if focused {
                "›"
            } else {
                "·"
            }
        } else {
            " "
        };
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
        let label = format!("{marker} {}", device.path);
        let style = if selected {
            RStyle::default().add_modifier(Modifier::REVERSED)
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
            let mut details = format!("  {status}");
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

/// The inner area of the stream pane (below its title line).
pub fn stream_body_area(area: Rect) -> Option<Rect> {
    if area.height < 3 || area.width < 20 {
        return None;
    }
    let main = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let sidebar_width = (main.width / 3).clamp(18, 32).min(main.width - 12);
    let stream = Rect::new(
        main.x + sidebar_width + 1,
        main.y,
        main.width - sidebar_width - 1,
        main.height,
    );
    if stream.height <= 1 || stream.width == 0 {
        return None;
    }
    Some(Rect::new(
        stream.x,
        stream.y + 1,
        stream.width,
        stream.height - 1,
    ))
}

fn draw_stream(state: &AppState, buffer: &mut Buffer, area: Rect) {
    let focused = state.focus == Focus::Stream;
    let border_style = if focused {
        RStyle::default()
            .fg(RColor::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        dim()
    };
    let title = match state.selected_session() {
        Some(session) => format!(
            " {} · {} baud · AI control: {} ",
            session.path,
            session.baud,
            session.control.as_deref().unwrap_or("none")
        ),
        None => " stream · no device open ".to_string(),
    };
    put(
        buffer,
        Rect::new(area.x, area.y, area.width, 1),
        0,
        &title,
        border_style,
    );
    if area.height <= 1 {
        return;
    }

    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    let Some(session) = state.selected_session() else {
        put(
            buffer,
            body,
            1,
            "Select a device in the sidebar and press Enter to open or join it.",
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
        return;
    }
    if let Some(notice) = &state.notice {
        let style = match notice.severity {
            Severity::Error => RStyle::default().fg(RColor::White).bg(RColor::Red),
            Severity::Info => RStyle::default().fg(RColor::Yellow),
        };
        put(buffer, area, 0, &format!(" {} ", notice.text), style);
        return;
    }
    put(
        buffer,
        area,
        0,
        " Tab: devices/stream · ↑/↓: select · Enter: open/join · b: baud · d: disconnect · Ctrl+Q: quit ",
        dim(),
    );
}

fn put(buffer: &mut Buffer, area: Rect, offset: u16, text: &str, style: RStyle) {
    let x = area.x + offset;
    let width = area.width.saturating_sub(offset);
    if area.height == 0 || width == 0 || x >= buffer.area.right() {
        return;
    }
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
    use crate::state::{AppState, DeviceEntry};
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
    fn unplug_notice_is_outside_the_stream() {
        let mut state = demo_state();
        state.apply_event(&Event::DeviceClosed {
            device_id: 7,
            path: "/dev/ttyUSB1".into(),
            reason: "device failed".into(),
        });
        let screen = lines(&state, 100, 16);
        let footer = screen.last().unwrap();
        assert!(footer.contains("unplugged"), "{footer}");
        assert!(footer.contains("/dev/ttyUSB1"), "{footer}");
        assert!(!screen[1..screen.len() - 1]
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
}
