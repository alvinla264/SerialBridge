//! A small ANSI terminal screen for the device stream.

use std::collections::VecDeque;

/// Rows of output the screen can keep above the live view before the oldest
/// are dropped.
pub const SCROLLBACK_ROWS: usize = 1_000;

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
    /// Rows that scrolled off the top, oldest first. Each row keeps the width
    /// it was written at, so a later resize cannot lose its text.
    history: VecDeque<Vec<Cell>>,
    /// How many history rows to keep; zero keeps none.
    scrollback: usize,
    /// How many rows the view is scrolled back from the live bottom.
    scroll: usize,
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
        Self::with_scrollback(rows, cols, 0)
    }

    /// A screen that keeps up to `scrollback` rows of scrolled-off output.
    pub fn with_scrollback(rows: usize, cols: usize, scrollback: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let mut screen = Self {
            rows,
            cols,
            grid: vec![Cell::default(); rows * cols],
            history: VecDeque::new(),
            scrollback: 0,
            scroll: 0,
            x: 0,
            y: 0,
            style: Style::default(),
            saved: (0, 0, Style::default()),
            pending_wrap: false,
            state: State::Ground,
            csi: Vec::new(),
            utf8_pending: Vec::new(),
        };
        screen.set_scrollback(scrollback);
        screen
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Keep up to `rows` rows of scrolled-off output. Zero keeps none and
    /// discards whatever was kept, so turning scrollback off frees it at once.
    pub fn set_scrollback(&mut self, rows: usize) {
        self.scrollback = rows;
        while self.history.len() > rows {
            self.history.pop_front();
        }
        if rows == 0 {
            self.history.clear();
        }
        self.scroll = self.scroll.min(self.history.len());
    }

    /// Rows kept above the live view.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// How many rows the view is scrolled back from the live bottom.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.scroll > 0
    }

    /// Scroll the view back by `rows`, stopping at the oldest kept row.
    pub fn scroll_back(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows).min(self.history.len());
    }

    /// Scroll the view towards the live bottom.
    pub fn scroll_forward(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    /// Show the oldest kept row.
    pub fn scroll_to_top(&mut self) {
        self.scroll = self.history.len();
    }

    /// Return to the live view.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
    }

    /// One cell of the view as it is scrolled: the live screen at
    /// [`Screen::scroll`] `0`, the kept rows further back. Columns past a
    /// history row's own width are blank, so a narrow resize shows the start of
    /// an old line instead of dropping it.
    pub fn view_cell(&self, row: usize, col: usize) -> Cell {
        let first = (self.history.len() + self.rows).saturating_sub(self.scroll + self.rows);
        let index = first + row;
        match index.checked_sub(self.history.len()) {
            Some(grid_row) => self.cell(grid_row, col),
            None => self
                .history
                .get(index)
                .and_then(|line| line.get(col))
                .copied()
                .unwrap_or_default(),
        }
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
        self.scroll = self.scroll.min(self.history.len());
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
            self.keep_top_row();
            self.grid.drain(..self.cols);
            self.grid
                .extend(std::iter::repeat_n(Cell::default(), self.cols));
        } else {
            self.y += 1;
        }
    }

    /// Move the top row into the scrollback before it is scrolled off. A full
    /// history drops its oldest row, so memory stays bounded.
    fn keep_top_row(&mut self) {
        if self.scrollback == 0 {
            return;
        }
        while self.history.len() >= self.scrollback {
            self.history.pop_front();
        }
        self.history.push_back(self.grid[..self.cols].to_vec());
        // A view that is scrolled back stays on the same text as new rows
        // arrive, and stops at the oldest row still kept.
        if self.scroll > 0 {
            self.scroll = (self.scroll + 1).min(self.history.len());
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
            2 => self.grid.fill(Cell::default()),
            // 3 erases the saved lines too, as a real terminal does. The view
            // comes back to live: there is nothing left behind it.
            3 => {
                self.grid.fill(Cell::default());
                self.history.clear();
                self.scroll = 0;
            }
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

    /// One row of the view as it is scrolled, trailing blanks trimmed.
    fn view_line(screen: &Screen, row: usize) -> String {
        let mut line: String = (0..screen.cols())
            .map(|col| screen.view_cell(row, col).ch)
            .collect();
        while line.ends_with(' ') {
            line.pop();
        }
        line
    }

    fn view(screen: &Screen) -> Vec<String> {
        (0..screen.rows())
            .map(|row| view_line(screen, row))
            .collect()
    }

    #[test]
    fn scrollback_keeps_the_rows_that_scroll_off_the_top() {
        let mut screen = Screen::new(2, 4);
        screen.set_scrollback(10);
        screen.feed(b"abcdefgh");
        assert_eq!(text(&screen), ["abcd", "efgh"]);
        assert_eq!(screen.history_len(), 0, "nothing has scrolled off yet");

        screen.feed(b"ij");
        assert_eq!(text(&screen), ["efgh", "ij"]);
        assert_eq!(screen.history_len(), 1);
        assert_eq!(view(&screen), ["efgh", "ij"], "live view");
        screen.scroll_back(1);
        assert_eq!(view(&screen), ["abcd", "efgh"], "one line back");
        screen.scroll_back(99);
        assert_eq!(
            view(&screen),
            ["abcd", "efgh"],
            "scrolling past the oldest line stops there"
        );
    }

    #[test]
    fn scrollback_spans_more_than_the_screen() {
        let mut screen = Screen::new(2, 4);
        screen.set_scrollback(10);
        screen.feed(b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
        assert_eq!(screen.history_len(), 3);
        assert_eq!(view(&screen), ["dddd", "eeee"]);
        screen.scroll_back(1);
        assert_eq!(view(&screen), ["cccc", "dddd"]);
        screen.scroll_back(1);
        assert_eq!(view(&screen), ["bbbb", "cccc"]);
        screen.scroll_back(1);
        assert_eq!(view(&screen), ["aaaa", "bbbb"]);
    }

    #[test]
    fn scrollback_is_bounded_and_drops_the_oldest_rows() {
        let mut screen = Screen::new(1, 4);
        screen.set_scrollback(2);
        for word in [b"aaaa", b"bbbb", b"cccc"] {
            screen.feed(word);
            screen.feed(b"\r\n");
        }
        screen.feed(b"dddd");
        assert_eq!(screen.history_len(), 2, "the oldest row was dropped");
        screen.scroll_back(1);
        assert_eq!(view(&screen)[0], "cccc");
        screen.scroll_back(1);
        assert_eq!(view(&screen)[0], "bbbb");
    }

    #[test]
    fn scrollback_off_keeps_nothing_and_turning_it_off_frees_what_was_kept() {
        let mut screen = Screen::new(2, 4);
        screen.feed(b"abcdefghij");
        assert_eq!(screen.history_len(), 0, "off by default");
        assert_eq!(screen.view_cell(0, 0).ch, 'e', "the live view is all");

        screen.set_scrollback(10);
        screen.feed(b"klmnop");
        assert!(screen.history_len() > 0);
        screen.set_scrollback(0);
        assert_eq!(screen.history_len(), 0, "turning it off frees the rows");
        screen.scroll_back(3);
        assert_eq!(
            view(&screen),
            ["ijkl", "mnop"],
            "with no history there is nothing to scroll to"
        );
    }

    #[test]
    fn erasing_the_saved_lines_clears_the_scrollback_but_a_plain_clear_keeps_it() {
        let mut screen = Screen::new(1, 4);
        screen.set_scrollback(10);
        screen.feed(b"aaaa\n");
        assert_eq!(screen.history_len(), 1);
        screen.feed(b"\x1b[2J");
        assert_eq!(screen.history_len(), 1, "CSI 2J keeps the saved lines");
        screen.scroll_to_top();
        screen.feed(b"\x1b[3J");
        assert_eq!(screen.history_len(), 0, "CSI 3J erases them");
        assert_eq!(screen.scroll(), 0, "and the view goes back to live");
    }

    #[test]
    fn a_narrower_resize_shows_the_start_of_a_history_row() {
        let mut screen = Screen::new(2, 6);
        screen.set_scrollback(10);
        screen.feed(b"abcdefghijklm");
        assert_eq!(screen.history_len(), 1);
        screen.resize(2, 3);
        screen.scroll_back(1);
        assert_eq!(view_line(&screen, 0), "abc");
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
