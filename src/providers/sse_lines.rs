//! Reassembly of SSE lines across HTTP body chunks.
//!
//! A body chunk carries no line-boundary guarantee: one SSE event can be split
//! across two network reads, and a multi-byte UTF-8 character can be split
//! across two chunks. Code that parses each chunk on its own silently drops
//! both halves of a split line. [`SseLineBuffer`] holds the unfinished tail
//! and hands out only complete lines.

/// Buffers raw bytes and yields complete lines, without their `\n` / `\r\n`.
#[derive(Debug, Default)]
pub struct SseLineBuffer {
    pending: Vec<u8>,
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` and return every line it completed. Bytes after the last
    /// newline stay buffered for the next call. Splitting on the byte `\n` is
    /// UTF-8 safe: that byte never occurs inside a multi-byte sequence.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let Some(last_newline) = self.pending.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.pending.drain(..=last_newline).collect();
        let mut lines: Vec<String> = complete
            .split(|b| *b == b'\n')
            .map(|line| {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                String::from_utf8_lossy(line).into_owned()
            })
            .collect();
        // `complete` ends in `\n`, so `split` yields one trailing empty slice
        // that is not a line.
        lines.pop();
        lines
    }

    /// True when a partial line is still waiting for its newline.
    pub fn has_partial_line(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::SseLineBuffer;

    #[test]
    fn a_line_split_across_chunks_is_reassembled() {
        let mut buf = SseLineBuffer::new();
        assert!(buf.push(b"data: {\"a\":").is_empty());
        assert!(buf.has_partial_line());
        assert_eq!(
            buf.push(b"1}\n\ndata: [DONE]\n"),
            vec!["data: {\"a\":1}", "", "data: [DONE]"]
        );
        assert!(!buf.has_partial_line());
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        let text = "data: é\n".as_bytes();
        let (head, tail) = text.split_at(7);
        let mut buf = SseLineBuffer::new();
        assert!(buf.push(head).is_empty());
        assert_eq!(buf.push(tail), vec!["data: é"]);
    }

    #[test]
    fn crlf_line_endings_are_stripped() {
        let mut buf = SseLineBuffer::new();
        assert_eq!(
            buf.push(b"event: ping\r\ndata: x\r\n"),
            vec!["event: ping", "data: x"]
        );
    }
}
