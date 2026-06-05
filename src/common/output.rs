#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub transcript: Vec<u8>,
    pub status_code: Option<i32>,
    pub streamed: bool,
}

impl CommandOutput {
    pub(crate) fn success(stdout: impl Into<String>) -> Self {
        Self {
            transcript: stdout.into().into_bytes(),
            status_code: Some(0),
            streamed: false,
        }
    }

    pub(crate) fn failure(stderr: impl Into<String>) -> Self {
        Self {
            transcript: stderr.into().into_bytes(),
            status_code: Some(1),
            streamed: false,
        }
    }

    pub fn streamed(transcript: Vec<u8>, status_code: Option<i32>) -> Self {
        Self {
            transcript,
            status_code,
            streamed: true,
        }
    }

    pub fn transcript_lossy(&self) -> String {
        String::from_utf8_lossy(&self.transcript).into_owned()
    }
}
