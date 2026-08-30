use anyhow::{Context, Result};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    pub password: String,
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
}

fn path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no config directory")?
        .join("beam/config.json"))
}
