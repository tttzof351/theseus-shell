use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use time::OffsetDateTime;

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn default_tmp_dir() -> io::Result<PathBuf> {
    home_dir()
        .map(|home| home.join(".theseus").join("tmp"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

pub fn create_tmp_log_file() -> io::Result<(PathBuf, fs::File)> {
    let dir = default_tmp_dir()?;
    create_tmp_log_file_in(&dir)
}

pub fn create_tmp_log_file_in(dir: &Path) -> io::Result<(PathBuf, fs::File)> {
    fs::create_dir_all(dir)?;

    for _ in 0..16 {
        let path = dir.join(format!("{}_{}.log", file_timestamp(), unique_id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create unique tmp log file",
    ))
}

pub fn cleanup_expired_tmp_files_async(ttl_min: usize) {
    thread::spawn(move || {
        let _ = cleanup_expired_tmp_files(ttl_min);
    });
}

fn cleanup_expired_tmp_files(ttl_min: usize) -> io::Result<()> {
    let dir = default_tmp_dir()?;
    let ttl = Duration::from_secs((ttl_min as u64).saturating_mul(60));
    let now = SystemTime::now();

    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !is_managed_tmp_log(&path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).is_ok_and(|age| age > ttl) {
            let _ = fs::remove_file(path);
        }
    }

    Ok(())
}

fn is_managed_tmp_log(path: &Path) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some("log")
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn file_timestamp() -> String {
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

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let pid = std::process::id() as u128;
    let value = nanos ^ (pid << 64) ^ counter;
    format!("{:08x}", value as u32)
}
