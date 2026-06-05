use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::Serialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone)]
pub struct AppLogger {
    inner: Arc<LoggerInner>,
}

#[derive(Debug)]
struct LoggerInner {
    log_path: PathBuf,
    trajectory_path: PathBuf,
    lock: Mutex<()>,
}

impl AppLogger {
    pub fn start_session() -> io::Result<Self> {
        let logs_dir = default_logs_dir()?;
        fs::create_dir_all(&logs_dir)?;

        let timestamp = session_timestamp();
        let logger = Self {
            inner: Arc::new(LoggerInner {
                log_path: logs_dir.join(format!("{timestamp}_log.jsonl")),
                trajectory_path: logs_dir.join(format!("{timestamp}_trajectory.json")),
                lock: Mutex::new(()),
            }),
        };

        logger.event("info", "session_start", json!({}))?;
        Ok(logger)
    }

    pub fn log_path(&self) -> &Path {
        &self.inner.log_path
    }

    pub fn trajectory_path(&self) -> &Path {
        &self.inner.trajectory_path
    }

    pub fn event(&self, level: &str, event: &str, fields: Value) -> io::Result<()> {
        let _guard = self.lock();
        let entry = json!({
            "timestamp": event_timestamp(),
            "level": level,
            "event": event,
            "fields": fields,
        });
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.inner.log_path)?;

        serde_json::to_writer(&mut file, &entry).map_err(io::Error::other)?;
        file.write_all(b"\n")
    }

    pub fn write_trajectory<T: Serialize>(&self, messages: &T) -> io::Result<()> {
        let _guard = self.lock();
        let messages = serde_json::to_value(messages).map_err(io::Error::other)?;
        let snapshot = json!({
            "timestamp": event_timestamp(),
            "messages": messages,
        });
        let mut file = fs::File::create(&self.inner.trajectory_path)?;
        serde_json::to_writer_pretty(&mut file, &snapshot).map_err(io::Error::other)?;
        file.write_all(b"\n")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner
            .lock
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }
}

pub fn default_logs_dir() -> io::Result<PathBuf> {
    home_dir()
        .map(|home| home.join(".theseus").join("logs"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn session_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}-{:02}-{:02}-{:02}",
        now.year(),
        now.month() as u8,
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn event_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown-time".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn session_timestamp_is_file_name_safe() {
        let timestamp = session_timestamp();

        assert!(!timestamp.contains(':'));
        assert!(!timestamp.contains("UTC"));
        assert_eq!(timestamp.len(), "2026-05-30-19-18-27".len());
    }

    #[test]
    fn write_trajectory_preserves_image_data_urls_for_resume() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = env::temp_dir().join(format!("theseus-logging-test-{suffix}"));
        fs::create_dir_all(&temp).unwrap();
        let logger = AppLogger {
            inner: Arc::new(LoggerInner {
                log_path: temp.join("test_log.jsonl"),
                trajectory_path: temp.join("test_trajectory.json"),
                lock: Mutex::new(()),
            }),
        };
        let messages = json!([
            {
                "role": "tool",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": "data:image/jpeg;base64,abc"
                        }
                    }
                ]
            }
        ]);

        logger.write_trajectory(&messages).unwrap();

        let text = fs::read_to_string(logger.trajectory_path()).unwrap();
        assert!(text.contains("data:image/jpeg;base64,abc"));
        fs::remove_dir_all(temp).unwrap();
    }
}
