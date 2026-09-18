//! Finding an `AskUserQuestion` tool call. A streamed response arrives on the bus as transport pieces, not as
//! frames, so the pieces are reassembled into SSE events first and each event's data is parsed; a buffered
//! response is one document. The captured body is read once per event for the question's text.

use serde_json::Value;

/// The most a streaming response is buffered for reassembly. A frame larger than this is not a tool-use
/// block start, and a response that runs past it stops being scanned rather than growing without bound.
pub const SSE_BUFFER_MAX: usize = 64 * 1024;

const TOOL: &str = "AskUserQuestion";

fn is_question_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("tool_use")
        && block.get("name").and_then(Value::as_str) == Some(TOOL)
}

/// One SSE event's data, parsed: a `content_block_start` whose block is the AskUserQuestion tool. A text delta
/// that merely mentions the name is another event type and does not count.
pub fn question_in_event(data: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(data) else {
        return false;
    };
    v.get("type").and_then(Value::as_str) == Some("content_block_start")
        && v.get("content_block").is_some_and(is_question_block)
}

/// A buffered response, parsed as one document.
pub fn question_in_document(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    v.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(is_question_block))
}

/// A bounded reassembler for one streaming response: transport bytes in, the data of each completed event out.
#[derive(Default)]
pub struct SseCutter {
    buffer: Vec<u8>,
    overflowed: bool,
}

impl SseCutter {
    /// Appends `bytes` and returns the `data:` payloads of every event that is now complete, an event ending at
    /// a blank line and its `data:` lines joined by newlines as the SSE grammar says. Past the cap the response
    /// is abandoned: nothing more is returned and `overflowed` reports it.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        if self.overflowed {
            return Vec::new();
        }
        if self.buffer.len() + bytes.len() > SSE_BUFFER_MAX {
            self.overflowed = true;
            self.buffer.clear();
            return Vec::new();
        }
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(end) = find_blank_line(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..end.next).collect();
            let data: Vec<&[u8]> = frame[..end.at]
                .split(|b| *b == b'\n')
                .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                .filter_map(|line| line.strip_prefix(b"data:"))
                .map(|rest| rest.strip_prefix(b" ").unwrap_or(rest))
                .collect();
            if !data.is_empty() {
                out.push(data.join(&b'\n'));
            }
        }
        out
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }
}

struct BlankLine {
    /// Where the frame's text ends.
    at: usize,
    /// Where the next frame starts.
    next: usize,
}

/// The first `\n\n` or `\r\n\r\n` in the buffer.
fn find_blank_line(buffer: &[u8]) -> Option<BlankLine> {
    let lf = buffer
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|at| BlankLine { at, next: at + 2 });
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| BlankLine { at, next: at + 4 });
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.at <= b.at { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// The question in a captured response: a JSON body carries it in the tool-use block; a streamed body was
/// captured as its SSE text, where the block starts empty and its input arrives as `input_json_delta` fragments
/// on the block's index.
pub fn question_text(response_body: &[u8]) -> Option<String> {
    if let Ok(v) = serde_json::from_slice::<Value>(response_body) {
        let block = v
            .get("content")?
            .as_array()?
            .iter()
            .find(|b| is_question_block(b))?;
        return from_input(block.get("input")?);
    }
    let mut index: Option<i64> = None;
    let mut json = String::new();
    for line in response_body.split(|b| *b == b'\n') {
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        let at = v.get("index").and_then(Value::as_i64);
        match v.get("type").and_then(Value::as_str) {
            Some("content_block_start") if v["content_block"]["name"] == TOOL => index = at,
            Some("content_block_delta") if index.is_some() && at == index => {
                if let Some(part) = v["delta"]["partial_json"].as_str() {
                    json.push_str(part);
                }
            }
            Some("content_block_stop") if index.is_some() && at == index => break,
            _ => {}
        }
    }
    index?;
    from_input(&serde_json::from_str(&json).ok()?)
}

fn from_input(input: &Value) -> Option<String> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|q| q.first())
        .and_then(|q| q.get("question"))
        .or_else(|| input.get("question"))
        .and_then(Value::as_str)
        .map(String::from)
}
