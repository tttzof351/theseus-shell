//! Bounded Chat Completions stream parsing, without terminal or network access.

mod accumulator;
mod decoder;
mod reasoning;

pub(super) use accumulator::Accumulator;
pub(super) use decoder::Decoder;

use std::fmt;

pub(super) const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct StreamError {
    pub message: String,
    pub retryable: bool,
    pub provider: bool,
}

impl StreamError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            provider: false,
        }
    }

    pub fn provider(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
            provider: true,
        }
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for StreamError {}

impl From<serde_json::Error> for StreamError {
    fn from(error: serde_json::Error) -> Self {
        Self::protocol(format!("Invalid stream JSON: {error}"))
    }
}

#[derive(Default)]
struct Budget {
    used: usize,
}
impl Budget {
    fn reserve(&mut self, bytes: usize) -> Result<(), StreamError> {
        if bytes > MAX_RESPONSE_BYTES.saturating_sub(self.used) {
            return Err(StreamError::protocol("LLM response exceeds 32 MiB limit"));
        }
        self.used += bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
