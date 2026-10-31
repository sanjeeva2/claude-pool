//! Extracts token usage from responses *while they stream through*, without
//! buffering the stream. For SSE it only buffers the current partial line and
//! only JSON-parses `data:` lines that contain `"usage"` (found with SIMD
//! `memmem`), so content deltas cost a single scan.

use memchr::memmem;
use serde::Deserialize;

use crate::pricing::TokenUsage;

const MAX_LINE: usize = 1 << 20; // 1 MiB: a usage event is tiny; anything bigger is skipped.
const MAX_JSON_BODY: usize = 64 << 20;

#[derive(Debug, Default, Deserialize)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
}

#[derive(Debug, Deserialize)]
struct WithUsage {
    model: Option<String>,
    usage: Option<ApiUsage>,
    message: Option<Box<WithUsage>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Sse,
    Json,
    Off,
}

#[derive(Debug)]
pub struct UsageTap {
    mode: Mode,
    line: Vec<u8>,
    skipping_line: bool,
    body: Vec<u8>,
    finder: memmem::Finder<'static>,
    pub usage: TokenUsage,
    pub model: Option<String>,
    pub saw_usage: bool,
}

impl UsageTap {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            line: Vec::new(),
            skipping_line: false,
            body: Vec::new(),
            finder: memmem::Finder::new(b"\"usage\""),
            usage: TokenUsage::default(),
            model: None,
            saw_usage: false,
        }
    }

    pub fn mode_for(content_type: Option<&str>) -> Mode {
        match content_type {
            Some(ct) if ct.starts_with("text/event-stream") => Mode::Sse,
            Some(ct) if ct.starts_with("application/json") => Mode::Json,
            _ => Mode::Off,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        match self.mode {
            Mode::Off => {}
            Mode::Json => {
                if self.body.len() + chunk.len() <= MAX_JSON_BODY {
                    self.body.extend_from_slice(chunk);
                } else {
                    self.mode = Mode::Off;
                    self.body = Vec::new();
                }
            }
            Mode::Sse => self.feed_sse(chunk),
        }
    }

    fn feed_sse(&mut self, mut chunk: &[u8]) {
        while let Some(nl) = memchr::memchr(b'\n', chunk) {
            let (head, rest) = chunk.split_at(nl);
            chunk = &rest[1..];
            if self.skipping_line {
                self.skipping_line = false;
                self.line.clear();
                continue;
            }
            if self.line.is_empty() {
                self.handle_line(head);
            } else {
                self.line.extend_from_slice(head);
                let line = std::mem::take(&mut self.line);
                self.handle_line(&line);
                self.line = line;
                self.line.clear();
            }
        }
        if !chunk.is_empty() && !self.skipping_line {
            if self.line.len() + chunk.len() > MAX_LINE {
                self.skipping_line = true;
                self.line.clear();
            } else {
                self.line.extend_from_slice(chunk);
            }
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        if self.finder.find(data).is_none() {
            return;
        }
        if let Ok(ev) = serde_json::from_slice::<WithUsage>(data) {
            self.absorb(ev);
        }
    }

    fn absorb(&mut self, ev: WithUsage) {
        // `message_start` nests the message; `message_delta` has top-level usage.
        let (model, usage) = match ev.message {
            Some(m) => (m.model, m.usage),
            None => (ev.model, ev.usage),
        };
        if let Some(m) = model {
            self.model = Some(m);
        }
        let Some(u) = usage else { return };
        self.saw_usage = true;
        // Values are cumulative, so later events overwrite earlier ones.
        if let Some(v) = u.input_tokens {
            self.usage.input = v;
        }
        if let Some(v) = u.output_tokens {
            self.usage.output = v;
        }
        if let Some(v) = u.cache_read_input_tokens {
            self.usage.cache_read = v;
        }
        match u.cache_creation {
            Some(cc)
                if cc.ephemeral_5m_input_tokens.is_some()
                    || cc.ephemeral_1h_input_tokens.is_some() =>
            {
                self.usage.cache_write_5m = cc.ephemeral_5m_input_tokens.unwrap_or(0);
                self.usage.cache_write_1h = cc.ephemeral_1h_input_tokens.unwrap_or(0);
            }
            _ => {
                if let Some(v) = u.cache_creation_input_tokens {
                    self.usage.cache_write_5m = v;
                    self.usage.cache_write_1h = 0;
                }
            }
        }
    }

    /// Call once the body is complete (or the client went away).
    pub fn finish(&mut self) {
        match self.mode {
            Mode::Json => {
                let body = std::mem::take(&mut self.body);
                if let Ok(ev) = serde_json::from_slice::<WithUsage>(&body) {
                    self.absorb(ev);
                }
            }
            Mode::Sse if !self.line.is_empty() && !self.skipping_line => {
                let line = std::mem::take(&mut self.line);
                self.handle_line(&line);
            }
            _ => {}
        }
        self.mode = Mode::Off;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":7,\"output_tokens\":1}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    fn expected() -> TokenUsage {
        TokenUsage {
            input: 12,
            output: 42,
            cache_read: 100,
            cache_write_5m: 7,
            cache_write_1h: 0,
        }
    }

    #[test]
    fn sse_whole() {
        let mut t = UsageTap::new(Mode::Sse);
        t.feed(SSE.as_bytes());
        t.finish();
        assert_eq!(t.usage, expected());
        assert_eq!(t.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn sse_split_at_every_byte() {
        let mut t = UsageTap::new(Mode::Sse);
        for b in SSE.as_bytes() {
            t.feed(std::slice::from_ref(b));
        }
        t.finish();
        assert_eq!(t.usage, expected());
    }

    #[test]
    fn sse_crlf() {
        let mut t = UsageTap::new(Mode::Sse);
        t.feed(SSE.replace('\n', "\r\n").as_bytes());
        t.finish();
        assert_eq!(t.usage, expected());
    }

    #[test]
    fn json_body_with_cache_split() {
        let mut t = UsageTap::new(Mode::Json);
        t.feed(br#"{"id":"m","model":"claude-sonnet-5-5","content":[],"usage":{"input_tokens":5,"output_tokens":9,"cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":4}}}"#);
        t.finish();
        assert_eq!(
            t.usage,
            TokenUsage {
                input: 5,
                output: 9,
                cache_read: 0,
                cache_write_5m: 3,
                cache_write_1h: 4
            }
        );
        assert_eq!(t.model.as_deref(), Some("claude-sonnet-5-5"));
    }
}
