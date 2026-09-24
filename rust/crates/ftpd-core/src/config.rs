use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: IpAddr,
    pub port: u16,
    pub max_connections: usize,
    pub idle_timeout_secs: u64,
    pub welcome_message: String,
    pub goodbye_message: String,
    pub passive_ports: PassivePorts,
    pub users: Vec<UserConfig>,
}

/// min = 0 (default) lets the OS assign an ephemeral port.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct PassivePorts {
    pub min: u16,
    pub max: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UserConfig {
    pub username: String,
    pub password: String,
    pub home: PathBuf,
    pub permissions: Permissions,
}

/// Directory permissions, mirroring the original six toggles.
/// `list` is derived: granted when either download or upload is allowed.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct Permissions {
    pub download: bool,
    pub upload: bool,
    pub rename: bool,
    pub delete: bool,
    pub mkdir: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 21,
            max_connections: 100,
            idle_timeout_secs: 300,
            welcome_message: "Welcome to mini-ftpd!".to_string(),
            goodbye_message: "Goodbye!".to_string(),
            passive_ports: PassivePorts::default(),
            users: vec![UserConfig::default()],
        }
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            username: "anonymous".to_string(),
            password: String::new(),
            home: PathBuf::from("./ftp_root"),
            permissions: Permissions::default(),
        }
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            download: true,
            upload: false,
            rename: false,
            delete: false,
            mkdir: false,
        }
    }
}

impl Permissions {
    pub fn allows(self, op: crate::fs::Operation) -> bool {
        use crate::fs::Operation::*;
        match op {
            Download => self.download,
            Upload => self.upload,
            Rename => self.rename,
            Delete => self.delete,
            Mkdir => self.mkdir,
            List => self.download || self.upload,
        }
    }
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.port == 0 {
            return Err(ConfigError::Invalid("port must be > 0".into()));
        }
        if self.users.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[users]] entry is required".into(),
            ));
        }
        let (min, max) = (self.passive_ports.min, self.passive_ports.max);
        if min != 0 && (min < 1024 || min > max) {
            return Err(ConfigError::Invalid(
                "passive_ports must be 0 (OS-assigned) or 1024 <= min <= max".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_original() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.port, 21);
        assert_eq!(cfg.max_connections, 100);
        assert_eq!(cfg.users[0].username, "anonymous");
        assert!(cfg.users[0].permissions.download);
        assert!(!cfg.users[0].permissions.upload);
    }

    #[test]
    fn parse_full_config() {
        let text = r#"
            port = 2121
            welcome_message = "hi"
            [passive_ports]
            min = 60000
            max = 60100
            [[users]]
            username = "bob"
            password = "secret"
            home = "/srv/ftp"
            [users.permissions]
            upload = true
            delete = true
        "#;
        let cfg: ServerConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.port, 2121);
        assert_eq!(cfg.passive_ports.min, 60000);
        assert!(cfg.users[0].permissions.upload);
        assert!(!cfg.users[0].permissions.rename);
        cfg.validate().unwrap();
    }
}
