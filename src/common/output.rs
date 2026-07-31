#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub transcript: Vec<u8>,
    pub status_code: Option<i32>,
    pub streamed: bool,
}

impl CommandOutput {
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
