use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use super::{AgentConfig, ConfigInit};

impl AgentConfig {
    pub fn load_or_create_default() -> io::Result<ConfigInit> {
        let path = default_config_path()?;
        Self::load_or_create_at(path)
    }

    pub fn load_or_create_at(path: impl Into<PathBuf>) -> io::Result<ConfigInit> {
        let path = path.into();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }

            let config = Self::default_empty();
            fs::write(&path, config.to_jsonc())?;

            return Ok(ConfigInit {
                config,
                path,
                created: true,
            });
        }

        let text = fs::read_to_string(&path)?;
        let config = Self::from_jsonc(&text)?;

        Ok(ConfigInit {
            config,
            path,
            created: false,
        })
    }

    pub fn save_at(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(path, self.to_jsonc())
    }
}

pub fn default_config_path() -> io::Result<PathBuf> {
    home_dir()
        .map(|home| home.join(".theseus").join("config.jsonc"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}
