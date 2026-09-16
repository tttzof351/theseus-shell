//! Real HTTP → AgentWorker → terminal checks. Every response boundary is held
//! by the test until the relevant rendered state or side effect is observed.
use super::*;
use serde_json::{Value, json};

fn tool_delta(command: &str) -> Value {
    json!({"role":"assistant","tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"bash","arguments":json!({"command":command}).to_string()}}]})
}

#[path = "streaming/server.rs"]
mod server;
#[path = "streaming/ui.rs"]
mod ui;
use server::{Action, SseServer};
use ui::{Ui, assert_bold_marker, assert_minimal_progress, normal_buffer_text, spinner};
#[path = "streaming/lifecycle.rs"]
mod lifecycle;
#[path = "streaming/plain.rs"]
mod plain;
#[path = "streaming/rendering.rs"]
mod rendering;
#[path = "streaming/tools.rs"]
mod tools;

fn log_events(home: &Path) -> io::Result<Vec<Value>> {
    let mut events = Vec::new();
    for file in fs::read_dir(home.join(".theseus/logs"))? {
        let file = file?;
        if file.file_name().to_string_lossy().ends_with("_log.jsonl") {
            let text = fs::read_to_string(file.path())?;
            // The final line may still be in the middle of the logger's write.
            events.extend(
                text.split_inclusive('\n')
                    .filter(|line| line.ends_with('\n'))
                    .map(serde_json::from_str)
                    .collect::<Result<Vec<Value>, _>>()?,
            );
        }
    }
    Ok(events)
}
