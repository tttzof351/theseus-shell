//! Optional export of real PTY bytes for tests/xterm. Normal cargo tests need no Node.
use super::*;
use serde_json::{Value, json};
use std::cell::RefCell;

#[derive(Default)]
pub(super) struct Trace {
    events: RefCell<Vec<Value>>,
}

impl Trace {
    pub fn checkpoint(&self, offset: usize, name: &str) {
        self.events.borrow_mut().push(json!({
            "offset": offset, "type": "checkpoint", "name": name,
        }));
    }

    pub fn resize(&self, offset: usize, rows: u16, cols: u16) {
        self.events.borrow_mut().push(json!({
            "offset": offset, "type": "resize", "rows": rows, "cols": cols,
        }));
    }

    pub fn export(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let Some(directory) = std::env::var_os("THESEUS_XTERM_TRACE_DIR") else {
            return Ok(());
        };
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(format!("{name}.ansi")), bytes)?;
        fs::write(
            directory.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "rows": SIZE.rows,
                "cols": SIZE.cols,
                "events": self.events.borrow().as_slice(),
            }))?,
        )
    }
}
