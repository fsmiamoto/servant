//! Configuration loader and path helpers.
//!
//! Path policy: we use `directories::BaseDirs` for portability where it
//! gives us the same answer as `$HOME`/XDG (the only platform we support
//! is Linux). State lives at `~/.servant/` and config at
//! `~/.config/servant/` — both rooted at the user's home directory, not at
//! XDG_STATE_HOME, because:
//!
//! - `~/.servant/` is documented and discoverable for the human user.
//! - keeping state and the control socket under a single 0700 dir makes
//!   single-user enforcement obvious.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::ttl::Ttl;

pub const DEFAULT_PORT: u16 = 4769;
pub const DEFAULT_BIND: &str = "0.0.0.0";
pub const DEFAULT_TTL: Ttl = Ttl::Duration(Duration::from_secs(24 * 3600));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_ttl")]
    pub default_ttl: Ttl,
    #[serde(default)]
    pub public_host: Option<String>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_bind() -> String {
    DEFAULT_BIND.into()
}
fn default_ttl() -> Ttl {
    DEFAULT_TTL
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            bind: DEFAULT_BIND.into(),
            default_ttl: DEFAULT_TTL,
            public_host: None,
        }
    }
}

impl Config {
    /// Load config from the canonical path; missing file → defaults.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_file_path()?)
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn state_dir(&self) -> PathBuf {
        state_dir().expect("home dir")
    }
    pub fn registry_db_path(&self) -> PathBuf {
        self.state_dir().join("registry.db")
    }
    pub fn control_socket_path(&self) -> PathBuf {
        self.state_dir().join("control.sock")
    }
    pub fn daemon_lock_path(&self) -> PathBuf {
        self.state_dir().join("daemon.lock")
    }

    pub fn ensure_state_dir(&self) -> Result<()> {
        let dir = self.state_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating state dir {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    pub fn ensure_config_dir(&self) -> Result<()> {
        let dir = config_dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }
}

pub fn home_dir() -> Result<PathBuf> {
    // Read $HOME first so test harnesses (HOME=tmpdir) get isolation; fall
    // back to BaseDirs if unset.
    if let Some(h) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(h));
    }
    BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("could not resolve home directory")
}

pub fn state_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".servant"))
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config").join("servant"))
}

pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn systemd_unit_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".config")
        .join("systemd")
        .join("user")
        .join("servant.service"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_when_no_file() {
        let cfg = Config::load_from(std::path::Path::new("/nonexistent/x.toml")).unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.bind, DEFAULT_BIND);
    }

    #[test]
    fn loads_override() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "port = 5000\nbind = \"127.0.0.1\"\ndefault_ttl = \"1h\"").unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.default_ttl, Ttl::Duration(Duration::from_secs(3600)));
    }

    #[test]
    fn never_ttl_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "default_ttl = \"never\"\n").unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg.default_ttl, Ttl::Never);
    }

    #[test]
    fn malformed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "port = \"abc\"\n").unwrap();
        assert!(Config::load_from(&p).is_err());
    }
}
