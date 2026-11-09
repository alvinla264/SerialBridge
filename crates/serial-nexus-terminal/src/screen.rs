//! A small ANSI terminal screen for the device stream.

/// A colour the device asked for. `Default` means the terminal default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Display attributes of one screen cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// One character cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: Style::default(),
        }
    }
}

impl Color {
    fn sgr_fg(code: u16) -> Option<Self> {
        Some(match code {
            30..=37 => Color::Indexed((code - 30) as u8),
            39 => Color::Default,
            90..=97 => Color::Indexed((code - 90 + 8) as u8),
            _ => return None,
        })
    }

    fn sgr_bg(code: u16) -> Option<Self> {
        Some(match code {
            40..=47 => Color::Indexed((code - 40) as u8),
            49 => Color::Default,
            100..=107 => Color::Indexed((code - 100 + 8) as u8),
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Csi,
    Osc,
    /// OSC ended by ESC; swallow the following `\` of the string terminator.
    OscEscape,
    /// `ESC (` and friends: the next byte names a character set and is ignored.
    Charset,
}

/// A minimal VT100/xterm-compatible screen: printable text, CR/LF/BS/TAB,
/// cursor movement, erase, save/restore, and SGR colour/attributes.
///
/// Unknown or private escape sequences are consumed and ignored, so a device
/// that emits a sequence this screen does not implement cannot corrupt it.
pub struct Screen {
    rows: usize,
    cols: usize,
    grid: Vec<Cell>,
    x: usize,
    y: usize,
    style: Style,
    saved: (usize, usize, Style),
    /// True after writing in the last column: the next glyph wraps first.
    pending_wrap: bool,
    state: State,
    csi: Vec<u8>,
    utf8_pending: Vec<u8>,
}

impl Screen {
    pub fn new(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            grid: vec![Cell::default(); rows * cols],
            x: 0,
            y: 0,
            style: Style::default(),
            saved: (0, 0, Style::default()),
            pending_wrap: false,
            state: State::Ground,
            csi: Vec::new(),
            utf8_pending: Vec::new(),
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The visible rows as strings with trailing blanks trimmed.
    pub fn lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|row| {
                let mut line: String = (0..self.cols)
                    .map(|col| self.grid[row * self.cols + col].ch)
                    .collect();
                while line.ends_with(' ') {
                    line.pop();
                }
                line
            })
            .collect()
    }

    /// Where the device's cursor is: (row, column). A cursor waiting to wrap
    /// stays on the last column.
    pub fn cursor(&self) -> (usize, usize) {
        (self.y, self.x)
    }

    pub fn cell(&self, row: usize, col: usize) -> Cell {
        if row < self.rows && col < self.cols {
            self.grid[row * self.cols + col]
        } else {
            Cell::default()
        }
    }

    fn index(&self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }

    /// Resize the visible screen, keeping the top-left content.
    pub fn resize(&mut self, rows: usize, cols: usize) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        let mut grid = vec![Cell::default(); rows * cols];
        for row in 0..rows.min(self.rows) {
            for col in 0..cols.min(self.cols) {
                grid[row * cols + col] = self.grid[self.index(row, col)];
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.grid = grid;
        self.x = self.x.min(cols - 1);
        self.y = self.y.min(rows - 1);
        self.pending_wrap = false;
    }

    /// Feed raw device bytes through the escape parser.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                State::Ground => self.ground(byte),
                State::Escape => self.escape(byte),
                State::Csi => self.csi(byte),
                State::Osc => match byte {
                    0x07 => self.state = State::Ground,
                    0x1b => self.state = State::OscEscape,
                    _ => {}
                },
                State::OscEscape => {
                    self.state = State::Ground;
                    // Swallow the `\` terminator; anything else is lost, which
                    // is harmless for an unsupported title sequence.
                    let _ = byte;
                }
                State::Charset => {
                    // The byte names the character set; switching is not
                    // implemented, but the whole sequence must be consumed.
                    let _ = byte;
                    self.state = State::Ground;
                }
            }
        }
    }

    /// Echo human keystrokes into the local view. The bytes come from
    /// [`crate::input::echo_key`]: printable text plus CR, LF, BS and TAB only.
    ///
    /// Echo has its own path, not [`Screen::feed`]: the device parser may be
    /// in the middle of an escape sequence or a UTF-8 sequence, and typed keys
    /// must not complete (or corrupt) it. Typed text also always uses the
    /// default style, so it never borrows the device's colours.
    pub fn echo(&mut self, bytes: &[u8]) {
        for ch in String::from_utf8_lossy(bytes).chars() {
            match ch {
                '\r' => {
                    self.x = 0;
                    self.pending_wrap = false;
                }
                '\n' => self.line_feed(),
                '\t' => {
                    self.pending_wrap = false;
                    let next = (self.x / 8 + 1) * 8;
                    self.x = next.min(self.cols - 1);
                }
                '\x08' => self.echo_backspace(),
                c if c.is_control() => {}
                c => self.put(c, Style::default()),
            }
        }
    }

    /// Erase the character before the cursor. At the right margin the cursor
    /// has not advanced yet (`pending_wrap`), so the last written cell is the
    /// one to erase.
    fn echo_backspace(&mut self) {
        if !self.pending_wrap && self.x > 0 {
            self.x -= 1;
        }
        self.pending_wrap = false;
        let index = self.index(self.y, self.x);
        self.grid[index] = Cell::default();
    }

    /// Show an AI write inline in the stream, in a distinct style. Control
    /// bytes are printed literally so they are visible and cannot drive the
    /// screen, and no marker or extra line break is inserted: the stream keeps
    /// the device's own layout.
    pub fn feed_ai(&mut self, bytes: &[u8]) {
        let ai = Style {
            fg: Some(Color::Indexed(11)),
            bold: true,
            ..Style::default()
        };
        let text = String::from_utf8_lossy(bytes);
        for ch in text.chars() {
            match ch {
                '\n' => {
                    self.put('\\', ai);
                    self.put('n', ai);
                }
                '\r' => {
                    self.put('\\', ai);
                    self.put('r', ai);
                }
                '\t' => {
                    self.put('\\', ai);
                    self.put('t', ai);
                }
                c if c.is_control() => {
                    for escaped in format!("\\x{:02x}", c as u32).chars() {
                        self.put(escaped, ai);
                    }
                }
                c => self.put(c, ai),
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        match byte {
            0x1b => {
                self.state = State::Escape;
                self.utf8_pending.clear();
            }
            b'\r' => {
                self.x = 0;
                self.pending_wrap = false;
            }
            b'\n' => self.line_feed(),
            0x08 => {
                self.pending_wrap = false;
                self.x = self.x.saturating_sub(1);
            }
            b'\t' => {
                self.pending_wrap = false;
                let next = (self.x / 8 + 1) * 8;
                self.x = next.min(self.cols - 1);
            }
            0x07 | 0x0e | 0x0f => {}
            0x00..=0x1f => {}
            _ => self.utf8(byte),
        }
    }

    fn utf8(&mut self, byte: u8) {
        self.utf8_pending.push(byte);
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(_) => {
                    let text = String::from_utf8(std::mem::take(&mut self.utf8_pending))
                        .expect("validated utf8");
                    for ch in text.chars() {
                        self.put(ch, self.style);
                    }
                    return;
                }
                Err(error) if error.error_len().is_none() => {
                    // Incomplete sequence: keep buffering until the next chunk.
                    return;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    let len = error.error_len().unwrap_or(1);
                    let prefix = String::from_utf8_lossy(&self.utf8_pending[..valid]).into_owned();
                    for ch in prefix.chars() {
                        self.put(ch, self.style);
                    }
                    self.put('\u{fffd}', self.style);
                    // Drop only the invalid sequence; valid bytes after it are
                    // decoded again instead of being lost.
                    self.utf8_pending.drain(..valid + len);
                    if self.utf8_pending.is_empty() {
                        return;
                    }
                }
            }
        }
    }

    fn put(&mut self, ch: char, style: Style) {
        if self.pending_wrap {
            self.x = 0;
            self.line_feed();
        }
        let index = self.index(self.y, self.x);
        self.grid[index] = Cell { ch, style };
        if self.x + 1 >= self.cols {
            self.pending_wrap = true;
        } else {
            self.x += 1;
        }
    }

    fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.y + 1 >= self.rows {
            self.grid.drain(..self.cols);
            self.grid
                .extend(std::iter::repeat_n(Cell::default(), self.cols));
        } else {
            self.y += 1;
        }
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.csi.clear();
                self.state = State::Csi;
            }
            b']' => self.state = State::Osc,
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'#' => {
                self.state = State::Charset;
            }
            b'7' => {
                self.saved = (self.x, self.y, self.style);
                self.state = State::Ground;
            }
            b'8' => {
                self.x = self.saved.0.min(self.cols - 1);
                self.y = self.saved.1.min(self.rows - 1);
                self.style = self.saved.2;
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi(&mut self, byte: u8) {
        if (0x40..=0x7e).contains(&byte) {
            let params = std::mem::take(&mut self.csi);
            self.state = State::Ground;
            self.apply_csi(byte, &params);
        } else {
            // Bound the parameter buffer so a malformed sequence cannot grow
            // memory without limit.
            if self.csi.len() < 64 {
                self.csi.push(byte);
            }
        }
    }

    fn apply_csi(&mut self, final_byte: u8, params: &[u8]) {
        let raw = String::from_utf8_lossy(params);
        // Private modes (`?25l`, `?1049h`, ...) and intermediate bytes are
        // consumed but not implemented.
        let private = raw.starts_with('?');
        let text = raw.trim_start_matches(['?', '>', '!']);
        let numbers: Vec<Option<usize>> = text
            .split(';')
            .map(|part| part.parse::<usize>().ok())
            .collect();
        let number = |index: usize, default: usize| {
            numbers
                .get(index)
                .copied()
                .flatten()
                .filter(|value| *value != 0)
                .unwrap_or(default)
        };
        if matches!(
            final_byte,
            b'A' | b'B' | b'C' | b'D' | b'H' | b'f' | b'G' | b'`' | b'd' | b'J' | b'K' | b'u'
        ) {
            self.pending_wrap = false;
        }
        match final_byte {
            b'A' => self.y = self.y.saturating_sub(number(0, 1)),
            b'B' => self.y = self.y.saturating_add(number(0, 1)).min(self.rows - 1),
            b'C' => self.x = self.x.saturating_add(number(0, 1)).min(self.cols - 1),
            b'D' => self.x = self.x.saturating_sub(number(0, 1)),
            b'H' | b'f' => {
                self.y = number(0, 1).saturating_sub(1).min(self.rows - 1);
                self.x = number(1, 1).saturating_sub(1).min(self.cols - 1);
            }
            b'G' | b'`' => self.x = number(0, 1).saturating_sub(1).min(self.cols - 1),
            b'd' => self.y = number(0, 1).saturating_sub(1).min(self.rows - 1),
            b'J' => self.erase_display(numbers.first().copied().flatten().unwrap_or(0)),
            b'K' => self.erase_line(numbers.first().copied().flatten().unwrap_or(0)),
            b'm' => self.sgr(&numbers),
            b's' => self.saved = (self.x, self.y, self.style),
            b'u' => {
                self.x = self.saved.0.min(self.cols - 1);
                self.y = self.saved.1.min(self.rows - 1);
                self.style = self.saved.2;
            }
            // Cursor visibility, alternate screen, scrolling regions and
            // everything else: consumed, ignored.
            _ => {
                let _ = private;
            }
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            0 => {
                let start = self.index(self.y, self.x);
                for cell in &mut self.grid[start..] {
                    *cell = Cell::default();
                }
            }
            1 => {
                let end = self.index(self.y, self.x);
                for cell in &mut self.grid[..=end] {
                    *cell = Cell::default();
                }
            }
            2 | 3 => self.grid.fill(Cell::default()),
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let base = self.index(self.y, 0);
        let (start, end) = match mode {
            0 => (base + self.x, base + self.cols),
            1 => (base, (base + self.x + 1).min(base + self.cols)),
            2 => (base, base + self.cols),
            _ => return,
        };
        for cell in &mut self.grid[start..end] {
            *cell = Cell::default();
        }
    }

    fn sgr(&mut self, numbers: &[Option<usize>]) {
        let mut index = 0;
        while index < numbers.len() {
            let code = numbers[index].unwrap_or(0);
            match code {
                0 => self.style = Style::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                4 => self.style.underline = true,
                7 => self.style.reverse = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => {}
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                38 | 48 => {
                    // 38;5;n and 38;2;r;g;b
                    let colour = match numbers.get(index + 1).copied().flatten() {
                        Some(5) => numbers
                            .get(index + 2)
                            .copied()
                            .flatten()
                            .map(|value| Color::Indexed(value as u8)),
                        Some(2) => {
                            let red = numbers.get(index + 2).copied().flatten().unwrap_or(0);
                            let green = numbers.get(index + 3).copied().flatten().unwrap_or(0);
                            let blue = numbers.get(index + 4).copied().flatten().unwrap_or(0);
                            Some(Color::Rgb(red as u8, green as u8, blue as u8))
                        }
                        _ => None,
                    };
                    if let Some(colour) = colour {
                        if code == 38 {
                            self.style.fg = Some(colour);
                        } else {
                            self.style.bg = Some(colour);
                        }
                    }
                    // 38;5;n consumes two parameters; 38;2;r;g;b consumes four.
                    index += match numbers.get(index + 1).copied().flatten() {
                        Some(5) => 2,
                        Some(2) => 4,
                        _ => 0,
                    };
                }
                _ => {
                    if let Some(colour) = Color::sgr_fg(code as u16) {
                        self.style.fg = Some(colour);
                    } else if let Some(colour) = Color::sgr_bg(code as u16) {
                        self.style.bg = Some(colour);
                    }
                }
            }
            index += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(screen: &Screen) -> Vec<String> {
        screen.lines()
    }

    #[test]
    fn plain_text_and_crlf() {
        let mut screen = Screen::new(4, 20);
        screen.feed(b"hello\r\nworld");
        assert_eq!(
            text(&screen)[..2],
            ["hello".to_string(), "world".to_string()]
        );
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"progress....\rDone");
        assert_eq!(text(&screen)[0], "Doneress....");
    }

    #[test]
    fn backspace_moves_left() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"abc\x08X");
        assert_eq!(text(&screen)[0], "abX");
    }

    #[test]
    fn wraps_and_scrolls_at_the_bottom() {
        let mut screen = Screen::new(2, 4);
        screen.feed(b"abcdefgh");
        assert_eq!(text(&screen), ["abcd", "efgh"]);
        screen.feed(b"ij");
        assert_eq!(text(&screen), ["efgh", "ij"]);
    }

    #[test]
    fn cursor_moves_and_erases() {
        let mut screen = Screen::new(3, 20);
        screen.feed(b"one\r\ntwo\r\nthree");
        screen.feed(b"\x1b[2;2HX");
        assert_eq!(text(&screen)[1], "tXo");
        screen.feed(b"\x1b[2K");
        assert_eq!(text(&screen)[1], "");
        screen.feed(b"\x1b[2J");
        assert!(text(&screen).iter().all(|line| line.is_empty()));
    }

    #[test]
    fn erase_to_end_of_line_keeps_prefix() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"abcdef\x1b[3G\x1b[K");
        assert_eq!(text(&screen)[0], "ab");
    }

    #[test]
    fn sgr_sets_colours_and_attributes() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"\x1b[1;31;44mR\x1b[0mN\x1b[38;5;200mP\x1b[38;2;1;2;3mT");
        assert_eq!(text(&screen)[0], "RNPT");
        assert_eq!(screen.cell(0, 0).style.fg, Some(Color::Indexed(1)));
        assert_eq!(screen.cell(0, 0).style.bg, Some(Color::Indexed(4)));
        assert!(screen.cell(0, 0).style.bold);
        assert_eq!(screen.cell(0, 1).style, Style::default());
        assert_eq!(screen.cell(0, 2).style.fg, Some(Color::Indexed(200)));
        assert_eq!(screen.cell(0, 3).style.fg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn unknown_escapes_and_osc_are_ignored() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"\x1b]0;title\x07ok\x1b[?25l\x1b[2 q!");
        assert_eq!(text(&screen)[0], "ok!");
    }

    #[test]
    fn utf8_split_across_feeds_is_kept_whole() {
        let mut screen = Screen::new(2, 20);
        screen.feed(&"é".as_bytes()[..1]);
        screen.feed(&"é".as_bytes()[1..]);
        assert_eq!(text(&screen)[0], "é");
    }

    #[test]
    fn invalid_utf8_becomes_replacement_character() {
        let mut screen = Screen::new(2, 20);
        screen.feed(&[0xff, b'a']);
        assert_eq!(text(&screen)[0], "�a");
    }

    #[test]
    fn an_invalid_utf8_prefix_does_not_drop_following_valid_text() {
        let mut screen = Screen::new(2, 20);
        screen.feed(&[0xc3, b'a', b'b']);
        assert_eq!(text(&screen)[0], "�ab");
    }

    #[test]
    fn charset_designator_is_consumed_completely() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"\x1b(Bready");
        assert_eq!(text(&screen)[0], "ready");
    }

    #[test]
    fn ai_writes_are_visible_unlabelled_and_keep_the_layout() {
        let mut screen = Screen::new(4, 40);
        screen.feed(b"boot> ");
        screen.feed_ai(b"AT\r");
        screen.feed(b"OK");
        let lines = text(&screen);
        assert_eq!(lines[0], "boot> AT\\rOK", "{lines:?}");
        assert!(!lines[0].contains("[AI]"), "{lines:?}");
    }

    #[test]
    fn local_echo_leaves_a_pending_device_sequence_alone() {
        let mut screen = Screen::new(4, 40);
        screen.feed(b"prompt> \x1b[3");
        screen.echo(b"A");
        screen.feed(b"1mhello");
        let lines = text(&screen);
        // The typed key sits where the device cursor was, and the device's own
        // text carries on after it in the colour its sequence asked for.
        assert_eq!(lines[0], "prompt> Ahello", "{lines:?}");
        assert_eq!(
            screen.cell(0, 8).style.fg,
            None,
            "typed text keeps the default style"
        );
        assert_eq!(screen.cell(0, 9).style.fg, Some(Color::Indexed(1)));

        // The same for a device character split across two reads.
        let mut screen = Screen::new(2, 20);
        screen.feed(&[0xc3]);
        screen.echo(b"A");
        screen.feed(&[0xa9]);
        let lines = text(&screen);
        assert_eq!(lines[0], "Aé", "{lines:?}");
    }

    #[test]
    fn echo_backspace_erases_the_last_character_at_the_right_margin() {
        let mut screen = Screen::new(4, 4);
        screen.echo(b"abcd");
        screen.echo(b"\x08");
        screen.echo(b"X");
        assert_eq!(text(&screen)[0], "abcX");

        // …and in the middle of a line.
        let mut screen = Screen::new(2, 10);
        screen.echo(b"ab");
        screen.echo(b"\x08");
        screen.echo(b"c");
        assert_eq!(text(&screen)[0], "ac");
    }

    #[test]
    fn local_echo_keeps_typed_text_in_the_default_style() {
        let mut screen = Screen::new(4, 40);
        screen.feed(b"\x1b[31m");
        screen.feed("prompt> ".as_bytes());
        screen.echo(b"AB");
        screen.echo(b"\x08C");
        screen.echo(b"\r\n");
        let lines = text(&screen);
        assert_eq!(lines[0], "prompt> AC", "{lines:?}");
        assert_eq!(lines[1], "", "{lines:?}");

        // The device's red stays on for its own output after the echo.
        let echoed = screen.cell(0, 8);
        assert_eq!(echoed.ch, 'A');
        assert_eq!(echoed.style.fg, None, "typed text is not device-coloured");
        screen.feed(b"Z");
        assert_eq!(screen.cell(1, 0).style.fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn sgr_at_the_right_margin_does_not_lose_the_next_character() {
        let mut screen = Screen::new(2, 4);
        screen.feed(b"abcd\x1b[0me");
        assert_eq!(text(&screen), ["abcd", "e"]);
    }

    #[test]
    fn indexed_colour_does_not_skip_a_following_attribute() {
        let mut screen = Screen::new(2, 20);
        screen.feed(b"\x1b[38;5;196;0mX");
        assert_eq!(screen.cell(0, 0).style, Style::default());
        screen.feed(b"\x1b[48;5;22;1mY");
        assert_eq!(screen.cell(0, 1).style.bg, Some(Color::Indexed(22)));
        assert!(screen.cell(0, 1).style.bold);
    }

    #[test]
    fn huge_cursor_counts_clamp_instead_of_overflowing() {
        let mut screen = Screen::new(2, 4);
        screen.feed(b"x\x1b[18446744073709551615CY");
        assert_eq!(text(&screen), ["x  Y", ""]);
    }

    #[test]
    fn repeated_resize_to_the_same_size_keeps_the_pending_wrap() {
        let mut screen = Screen::new(2, 4);
        screen.feed(b"abcd");
        screen.resize(2, 4);
        screen.feed(b"e");
        assert_eq!(text(&screen), ["abcd", "e"]);
    }

    #[test]
    fn resize_keeps_top_left_content() {
        let mut screen = Screen::new(2, 5);
        screen.feed(b"abcde");
        screen.resize(3, 10);
        assert_eq!(text(&screen)[0], "abcde");
        screen.feed(b"fgh");
        assert_eq!(text(&screen)[0], "abcdfgh");
        assert_eq!(screen.cols(), 10);
    }
}
