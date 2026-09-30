use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    pub password: String,
    pub admin_token: String,
    #[serde(default = "default_persistent_sessions")]
    pub persistent_sessions: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = path()?;
        if path.exists() {
            return serde_json::from_slice(&fs::read(&path)?).context("invalid config file");
        }
        let config = Self {
            password: rand::rng()
                .sample_iter(Alphanumeric)
                .take(24)
                .map(char::from)
                .collect(),
            admin_token: admin_token(),
            persistent_sessions: default_persistent_sessions(),
        };
        config.save()?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let path = path()?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn settings_opened() -> Result<bool> {
        Ok(settings_marker()?.exists())
    }

    pub fn mark_settings_opened() -> Result<()> {
        let marker = settings_marker()?;
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(marker, b"opened")?;
        Ok(())
    }
}

fn default_persistent_sessions() -> bool {
    cfg!(target_os = "linux")
}

fn admin_token() -> String {
    let mut bytes = [0; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn path() -> Result<PathBuf> {
    Ok(config_directory()?.join("beam/config.json"))
}

fn settings_marker() -> Result<PathBuf> {
    Ok(config_directory()?.join("beam/settings-opened"))
}

fn config_directory() -> Result<PathBuf> {
    dirs::config_dir().context("no config directory")
}
