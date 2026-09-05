//! SSE frame decoding.
//!
//! Ported from `src-tauri/src/bigtiny/stream.rs` rather than rewritten. The
//! resume-offset scanning and the decode-once-per-frame rule below are subtle,
//! were arrived at by fixing real bugs, and are exactly the kind of thing three
//! hand-rolled client copies would each get wrong differently.
//!
//! Extended for V2 with `id:`, which `Last-Event-ID` resumes from.

use bigtiny2_protocol::SSEEvent;

/// One decoded frame: the event, and the id to resume from if it carried one.
#[derive(Debug, Clone)]
pub struct Frame {
    pub id: Option<u64>,
    pub event: SSEEvent,
}

/// Byte offset of the first `"\n\n"` terminator at or after `from`.
///
/// Operates on raw bytes rather than `str::find` on a `&str`, so the caller can
/// resume from an arbitrary offset without needing it to land on a UTF-8 char
/// boundary: `\n` is single-byte ASCII and never appears inside a multi-byte
/// sequence, so a boundary found here always splits *between* characters.
pub fn find_frame_boundary(haystack: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|rel| from + rel)
}

/// Drain every complete frame in `buffer`, carrying the scan offset in
/// `scan_from` between calls.
///
/// Without the resume offset, every incoming chunk would re-scan the whole
/// accumulated buffer from zero — quadratic when a large delta arrives as many
/// small TCP chunks.
///
/// Decoding happens only once a frame is *complete*, never per chunk, so a
/// multi-byte character split across chunks is decoded after all its bytes have
/// arrived. Decoding per chunk mangles it into replacement characters that the
/// JSON parser then silently rejects, dropping the whole frame. A complete
/// frame that is still not valid UTF-8 is a daemon bug: drop it loudly rather
/// than lossy-decode. Its terminator is already consumed, so the stream
/// continues cleanly from the next frame.
pub fn drain_complete_frames(buffer: &mut Vec<u8>, scan_from: &mut usize) -> Vec<String> {
    let mut frames = Vec::new();
    // `saturating_sub(1)` guards the one case a pure resume point would miss: a
    // terminator split exactly across the chunk join, with one `\n` at the end
    // of the scanned region and the second at the start of the new tail.
    while let Some(pos) = find_frame_boundary(buffer, scan_from.saturating_sub(1)) {
        let frame_bytes: Vec<u8> = buffer.drain(..pos + 2).collect();
        *scan_from = 0;
        match std::str::from_utf8(&frame_bytes[..frame_bytes.len() - 2]) {
            Ok(frame) => frames.push(frame.to_string()),
            Err(e) => tracing::warn!("dropping a non-UTF-8 SSE frame ({e})"),
        }
    }
    *scan_from = buffer.len();
    frames
}

/// Parse one frame into an event plus its resume id.
///
/// `data:` lines are concatenated per the SSE spec — a large payload may be
/// split across several. A frame with no parseable `data:` yields `None`;
/// comments and keep-alives are legitimately empty.
pub fn parse_frame(frame: &str) -> Option<Frame> {
    let mut data = String::new();
    let mut id = None;

    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = rest.trim().parse::<u64>().ok();
        }
    }

    if data.is_empty() {
        return None;
    }
    let event = serde_json::from_str::<SSEEvent>(&data).ok()?;
    Some(Frame { id, event })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boundary_is_found_and_resumed_from() {
        assert_eq!(find_frame_boundary(b"data: a\n\ndata: b\n\n", 0), Some(7));
        // Resuming past the first finds the second, not the first again.
        assert_eq!(find_frame_boundary(b"data: a\n\ndata: b\n\n", 9), Some(16));
        assert!(find_frame_boundary(b"data: a", 0).is_none());
        assert!(find_frame_boundary(b"", 0).is_none());
        assert!(find_frame_boundary(b"abc", 10).is_none());
    }

    #[test]
    fn a_terminator_split_across_chunks_is_still_found() {
        // The case `saturating_sub(1)` exists for: one `\n` already scanned,
        // the second arriving in the next chunk.
        let mut buffer = b"data: a\n".to_vec();
        let mut scan_from = 0usize;
        assert!(drain_complete_frames(&mut buffer, &mut scan_from).is_empty());
        assert_eq!(scan_from, buffer.len());

        buffer.extend_from_slice(b"\n");
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames, vec!["data: a".to_string()]);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        // Decoding per chunk would mangle this into replacement characters and
        // the JSON parse would then silently drop the frame.
        let payload = r#"data: {"type":"llm_delta","content":"café"}"#;
        let bytes = payload.as_bytes();
        let split = bytes.len() - 3; // mid "é"

        let mut buffer = bytes[..split].to_vec();
        let mut scan_from = 0usize;
        assert!(drain_complete_frames(&mut buffer, &mut scan_from).is_empty());

        buffer.extend_from_slice(&bytes[split..]);
        buffer.extend_from_slice(b"\n\n");
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames.len(), 1);

        let parsed = parse_frame(&frames[0]).expect("frame should parse");
        assert_eq!(parsed.event.content.as_deref(), Some("café"));
    }

    #[test]
    fn several_frames_drain_in_one_pass() {
        let mut buffer =
            br#"data: {"type":"llm_delta","content":"a"}

data: {"type":"llm_delta","content":"b"}

"#
            .to_vec();
        let mut scan_from = 0usize;
        let frames = drain_complete_frames(&mut buffer, &mut scan_from);
        assert_eq!(frames.len(), 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_id_line_is_read_as_the_resume_point() {
        let frame = "id: 42\ndata: {\"type\":\"tool_start\",\"tool_name\":\"read_file\"}";
        let parsed = parse_frame(frame).expect("should parse");
        assert_eq!(parsed.id, Some(42));
        assert_eq!(parsed.event.tool_name.as_deref(), Some("read_file"));
    }

    #[test]
    fn a_frame_without_an_id_still_parses() {
        // Text deltas are unnumbered on purpose: they are not replayable, so
        // an id a client could resume from would point at nothing.
        let parsed = parse_frame(r#"data: {"type":"llm_delta","content":"hi"}"#).unwrap();
        assert!(parsed.id.is_none());
    }

    #[test]
    fn data_lines_are_concatenated() {
        // A large payload may legitimately arrive across several `data:` lines.
        let frame = "data: {\"type\":\"llm_delta\",\ndata: \"content\":\"split\"}";
        let parsed = parse_frame(frame).expect("should parse");
        assert_eq!(parsed.event.content.as_deref(), Some("split"));
    }

    #[test]
    fn empty_and_unparseable_frames_are_skipped_rather_than_fatal() {
        assert!(parse_frame("").is_none());
        assert!(parse_frame(": keep-alive comment").is_none());
        assert!(parse_frame("data: not json").is_none());
    }
}
