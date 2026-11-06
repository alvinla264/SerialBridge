use std::collections::VecDeque;

use serialbridge_protocol::{HistoryLine, Source};

/// One stored line. `data` keeps the original bytes; text views are lossy.
#[derive(Clone, Debug)]
pub struct StoredLine {
    pub seq: u64,
    pub source: Source,
    pub data: Vec<u8>,
    pub partial: bool,
}

impl StoredLine {
    pub fn to_wire(&self) -> HistoryLine {
        HistoryLine {
            source: self.source,
            text: String::from_utf8_lossy(&self.data).into_owned(),
            partial: self.partial,
        }
    }
}

/// A bounded in-memory ring of lines for one open device.
///
/// When a line would exceed either the line or byte bound, the oldest lines
/// are dropped and counted, so clients can tell that history is incomplete.
pub struct History {
    lines: VecDeque<StoredLine>,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    dropped: u64,
    next_seq: u64,
}

impl History {
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            max_lines: max_lines.max(1),
            max_bytes: max_bytes.max(1),
            dropped: 0,
            next_seq: 0,
        }
    }

    /// Add a line (without its newline) and return its sequence number.
    pub fn push(&mut self, source: Source, data: Vec<u8>, partial: bool) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.bytes += data.len();
        self.lines.push_back(StoredLine {
            seq,
            source,
            data,
            partial,
        });
        while self.lines.len() > self.max_lines || self.bytes > self.max_bytes {
            if let Some(old) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.data.len());
                self.dropped += 1;
            } else {
                break;
            }
        }
        seq
    }

    /// The last `count` lines, oldest first.
    pub fn recent(&self, count: usize) -> Vec<StoredLine> {
        let skip = self.lines.len().saturating_sub(count);
        self.lines.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Take a sequence number for an open (partial) tail, so it keeps its
    /// chronological place among complete lines from any source.
    pub fn reserve_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_last_lines() {
        let mut history = History::new(3, 1024);
        for i in 0..5u8 {
            history.push(Source::Device, vec![b'0' + i], false);
        }
        let lines: Vec<String> = history
            .recent(10)
            .iter()
            .map(|line| String::from_utf8_lossy(&line.data).into_owned())
            .collect();
        assert_eq!(lines, ["2", "3", "4"]);
        assert_eq!(history.dropped(), 2);
    }

    #[test]
    fn byte_bound_drops_oldest() {
        let mut history = History::new(100, 10);
        history.push(Source::Device, vec![b'a'; 6], false);
        history.push(Source::Device, vec![b'b'; 6], false);
        assert_eq!(history.len(), 1);
        assert_eq!(history.dropped(), 1);
    }

    #[test]
    fn invalid_utf8_is_lossy_in_text_but_not_in_bytes() {
        let mut history = History::new(10, 1024);
        history.push(Source::Device, vec![0xff, 0xfe, b'x'], false);
        let stored = &history.recent(1)[0];
        assert_eq!(stored.data, vec![0xff, 0xfe, b'x']);
        assert!(stored.to_wire().text.contains('\u{fffd}'));
    }

    #[test]
    fn reserved_sequence_orders_a_tail_before_later_lines() {
        let mut history = History::new(10, 1024);
        let tail = history.reserve_seq();
        let written = history.push(Source::Ai, b"later".to_vec(), true);
        assert!(tail < written);
    }

    #[test]
    fn partial_lines_are_kept_and_marked() {
        let mut history = History::new(10, 1024);
        history.push(Source::Ai, b"no-newline".to_vec(), true);
        assert!(history.recent(1)[0].partial);
    }
}
