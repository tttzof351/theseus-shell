use super::{MAX_SSE_EVENT_BYTES, StreamError};

/// SSE line framing. Input slices may end anywhere, including inside UTF-8,
/// CRLF, BOM or a data field. Only a blank line dispatches an event.
pub(in crate::agent) struct Decoder {
    line: Vec<u8>,
    data: String,
    first_line: bool,
    skip_lf: bool,
    event_bytes: usize,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            first_line: true,
            skip_lf: false,
            event_bytes: 0,
        }
    }
}

impl Decoder {
    #[cfg(test)]
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, StreamError> {
        let mut events = Vec::new();
        let mut bytes = bytes;
        while let Some(event) = self.next_event(&mut bytes)? {
            events.push(event);
        }
        Ok(events)
    }

    /// Yield each complete event before inspecting later bytes. A malformed
    /// tail in the same HTTP chunk must not discard a previously valid prefix.
    pub fn next_event(&mut self, bytes: &mut &[u8]) -> Result<Option<String>, StreamError> {
        while let Some((&byte, tail)) = bytes.split_first() {
            *bytes = tail;
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if self.event_bytes >= MAX_SSE_EVENT_BYTES {
                return Err(StreamError::protocol("SSE event exceeds 8 MiB limit"));
            }
            self.event_bytes += 1;
            match byte {
                b'\r' | b'\n' => {
                    let event = self.end_line()?;
                    self.skip_lf = byte == b'\r';
                    if event.is_some() {
                        return Ok(event);
                    }
                }
                _ => self.line.push(byte),
            }
        }
        Ok(None)
    }

    fn end_line(&mut self) -> Result<Option<String>, StreamError> {
        let mut event = None;
        let line = std::str::from_utf8(&self.line)
            .map_err(|_| StreamError::protocol("Invalid UTF-8 in SSE event"))?;
        let line = if self.first_line {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        self.first_line = false;
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop(); // SSE inserts one LF per data field, including empty ones.
                event = Some(std::mem::take(&mut self.data));
            }
            self.event_bytes = 0;
        } else if !line.starts_with(':') {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            if name == "data" {
                self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
                self.data.push('\n');
            }
            // event/id/retry do not change POST parsing or enable reconnect.
        }
        self.line.clear();
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_split_preserves_utf8_bom_crlf_fields_and_event_boundaries() {
        let source = "\u{feff}: keepalive\r\ndata: Привет🙂\r\ndata:  second\r\n\r\nid: x\nretry: 1\nevent: message\ndata\n\ndata: end\r\rdata: incomplete";
        for split in 0..=source.len() {
            let mut decoder = Decoder::default();
            let mut events = decoder.push(&source.as_bytes()[..split]).unwrap();
            events.extend(decoder.push(&source.as_bytes()[split..]).unwrap());
            assert_eq!(events, ["Привет🙂\n second", "", "end"], "split {split}");
        }
        let mut decoder = Decoder::default();
        let events = source
            .as_bytes()
            .iter()
            .flat_map(|b| decoder.push(&[*b]).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events, ["Привет🙂\n second", "", "end"]);
    }

    #[test]
    fn eof_does_not_dispatch_and_bom_is_only_special_at_start() {
        let mut decoder = Decoder::default();
        assert!(decoder.push(b"data: orphan\n").unwrap().is_empty());
        assert_eq!(
            decoder.push("\ndata: \u{feff}kept\n\n".as_bytes()).unwrap(),
            ["orphan", "\u{feff}kept"]
        );
    }

    #[test]
    fn rejects_invalid_utf8_and_bounds_unknown_fields_too() {
        assert!(Decoder::default().push(b"data: \xff\n\n").is_err());
        let mut decoder = Decoder {
            event_bytes: MAX_SSE_EVENT_BYTES - 1,
            ..Default::default()
        };
        assert!(decoder.push(b"xx").is_err());
        assert_eq!(decoder.line.len(), 1);
    }

    #[test]
    fn valid_event_precedes_malformed_tail_in_the_same_read() {
        let mut decoder = Decoder::default();
        let mut bytes = b"data: prefix\r\n\r\ndata: \xff\n\n".as_slice();
        assert_eq!(
            decoder.next_event(&mut bytes).unwrap().as_deref(),
            Some("prefix")
        );
        assert!(decoder.next_event(&mut bytes).is_err());
    }

    #[test]
    fn actual_eight_mib_event_resets_limit_and_overflow_never_grows_the_buffer() {
        let mut bytes = vec![b'x'; MAX_SSE_EVENT_BYTES];
        bytes[..6].copy_from_slice(b"data: ");
        bytes[MAX_SSE_EVENT_BYTES - 2..].copy_from_slice(b"\n\n");
        let mut decoder = Decoder::default();
        let events = decoder.push(&bytes).unwrap();
        assert_eq!(events[0].len(), MAX_SSE_EVENT_BYTES - 8);
        assert_eq!(decoder.push(b"data: next\n\n").unwrap(), ["next"]);

        bytes.fill(b'x');
        assert!(decoder.push(&bytes).unwrap().is_empty());
        assert!(decoder.push(b"x").is_err());
        assert_eq!(decoder.line.len(), MAX_SSE_EVENT_BYTES);
    }
}
