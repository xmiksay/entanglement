//! Byte-level framing for SSE streams (#443).
//!
//! `spawn_byte_stream` forwards raw `reqwest` body chunks whose boundaries fall
//! on arbitrary bytes (TCP/HTTP-chunk framing), not character boundaries. A
//! multi-byte UTF-8 sequence (emoji, curly quotes, CJK, accented chars) can
//! therefore straddle two chunks. Decoding each chunk independently with
//! `String::from_utf8_lossy` — the previous approach — replaces the trailing
//! bytes of one chunk and the leading bytes of the next with U+FFFD each,
//! silently corrupting both the visible text and any tool-call JSON argument
//! fragment riding in the same buffer.
//!
//! [`SseFrameBuffer`] fixes this by accumulating raw bytes and decoding only
//! once a complete, delimiter-terminated frame is buffered. This is safe
//! because the delimiters used here (`\n`, `\n\n`) are pure ASCII, and the
//! byte `\n` (0x0A) can never appear inside a multi-byte UTF-8 sequence —
//! continuation bytes are always in the range 0x80-0xBF. So a frame boundary
//! never falls inside a character.

/// Accumulates raw bytes and yields complete frames terminated by `delimiter`,
/// decoding each frame only once it is whole.
pub struct SseFrameBuffer {
    buf: Vec<u8>,
    delimiter: &'static [u8],
}

impl SseFrameBuffer {
    pub fn new(delimiter: &'static [u8]) -> Self {
        Self {
            buf: Vec::new(),
            delimiter,
        }
    }

    /// Append raw bytes from the next network chunk.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame (bytes up to and including `delimiter`),
    /// lossily decoded as UTF-8. `None` when no complete frame is buffered yet.
    pub fn next_frame(&mut self) -> Option<String> {
        let idx = self
            .buf
            .windows(self.delimiter.len())
            .position(|w| w == self.delimiter)?;
        let frame_bytes: Vec<u8> = self.buf.drain(..idx + self.delimiter.len()).collect();
        Some(String::from_utf8_lossy(&frame_bytes).into_owned())
    }

    /// Drain whatever bytes remain with no trailing delimiter — call once at
    /// EOF (#483). A stream cut mid-frame, or a server that omits the final
    /// delimiter on its last event, leaves exactly this: a frame `next_frame`
    /// will never surface on its own. `None` when nothing but whitespace (or
    /// nothing at all) is left.
    pub fn take_remaining(&mut self) -> Option<String> {
        let bytes: Vec<u8> = std::mem::take(&mut self.buf);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_push_yields_complete_frame() {
        let mut buf = SseFrameBuffer::new(b"\n");
        buf.push(b"data: hello\n");
        assert_eq!(buf.next_frame().as_deref(), Some("data: hello\n"));
        assert!(buf.next_frame().is_none());
    }

    #[test]
    fn frame_split_across_pushes_with_no_multibyte_char_reassembles() {
        let mut buf = SseFrameBuffer::new(b"\n");
        buf.push(b"data: hel");
        assert!(buf.next_frame().is_none());
        buf.push(b"lo\n");
        assert_eq!(buf.next_frame().as_deref(), Some("data: hello\n"));
    }

    #[test]
    fn multibyte_char_split_mid_sequence_reassembles_losslessly() {
        // "🎉" (U+1F389) encodes to 4 bytes: F0 9F 8E 89. Split the chunk after
        // the first 2 bytes of the emoji, mimicking a chunk boundary landing
        // mid-character.
        let text = "before 🎉 after";
        let bytes = text.as_bytes();
        let emoji_start = text.find('🎉').expect("emoji present");
        let split_at = emoji_start + 2; // inside the 4-byte sequence

        let mut buf = SseFrameBuffer::new(b"\n");
        buf.push(&bytes[..split_at]);
        assert!(
            buf.next_frame().is_none(),
            "no delimiter seen yet, no frame should be popped"
        );
        buf.push(&bytes[split_at..]);
        buf.push(b"\n");

        let frame = buf.next_frame().expect("frame should be complete");
        assert_eq!(frame, format!("{text}\n"));
        assert!(
            !frame.contains('\u{FFFD}'),
            "reassembled frame must not contain a replacement character: {frame:?}"
        );
    }

    #[test]
    fn multibyte_char_split_inside_tool_call_argument_fragment() {
        // Simulates a `function.arguments` JSON fragment where the split lands
        // mid-character inside a non-ASCII string value (e.g. a city name).
        let json_line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"Curaçao\"}"}}]}}]}"#;
        let full = format!("{json_line}\n");
        let bytes = full.as_bytes();
        // "ç" (U+00E7) is 2 bytes (0xC3 0xA7); split right after the first byte.
        let c_cedilla_start = full.find('ç').expect("ç present");
        let split_at = c_cedilla_start + 1;

        let mut buf = SseFrameBuffer::new(b"\n");
        buf.push(&bytes[..split_at]);
        assert!(buf.next_frame().is_none());
        buf.push(&bytes[split_at..]);

        let frame = buf.next_frame().expect("frame should be complete");
        assert_eq!(frame, full);
        assert!(!frame.contains('\u{FFFD}'));
    }

    #[test]
    fn double_newline_delimiter_frames_correctly() {
        let mut buf = SseFrameBuffer::new(b"\n\n");
        buf.push(b"data: {\"a\":1}\n");
        assert!(
            buf.next_frame().is_none(),
            "only one of the two newlines seen"
        );
        buf.push(b"\ndata: {\"b\":2}\n\n");
        assert_eq!(buf.next_frame().as_deref(), Some("data: {\"a\":1}\n\n"));
        assert_eq!(buf.next_frame().as_deref(), Some("data: {\"b\":2}\n\n"));
        assert!(buf.next_frame().is_none());
    }

    #[test]
    fn multibyte_char_split_across_double_newline_frame() {
        // "…" (U+2026, horizontal ellipsis) is 3 bytes (E2 80 A6), split after
        // the first byte, landing inside a "\n\n"-delimited frame.
        let frame_text = "data: thinking…\n\n";
        let bytes = frame_text.as_bytes();
        let ellipsis_start = frame_text.find('…').expect("ellipsis present");
        let split_at = ellipsis_start + 1;

        let mut buf = SseFrameBuffer::new(b"\n\n");
        buf.push(&bytes[..split_at]);
        assert!(buf.next_frame().is_none());
        buf.push(&bytes[split_at..]);

        let frame = buf.next_frame().expect("frame should be complete");
        assert_eq!(frame, frame_text);
        assert!(!frame.contains('\u{FFFD}'));
    }

    #[test]
    fn take_remaining_yields_unterminated_trailing_bytes() {
        let mut buf = SseFrameBuffer::new(b"\n");
        buf.push(b"data: {\"a\":1}\n");
        buf.push(b"data: {\"b\":2}"); // no trailing newline — connection closed here
        assert_eq!(buf.next_frame().as_deref(), Some("data: {\"a\":1}\n"));
        assert!(buf.next_frame().is_none(), "second frame has no delimiter");
        assert_eq!(buf.take_remaining().as_deref(), Some("data: {\"b\":2}"));
        assert!(buf.take_remaining().is_none(), "drained, nothing left");
    }

    #[test]
    fn take_remaining_is_none_when_buffer_empty_or_whitespace() {
        let mut buf = SseFrameBuffer::new(b"\n");
        assert!(buf.take_remaining().is_none());
        buf.push(b"   ");
        assert!(buf.take_remaining().is_none());
    }
}
