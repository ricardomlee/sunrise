use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const DEFAULT_CONFIG_FILE: &str = "sunrise.toml";
pub const DEFAULT_HTTP_PORT: u16 = 47989;
pub const DEFAULT_HTTPS_PORT: u16 = 47984;
pub const DEFAULT_RTSP_PORT: u16 = 48010;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config at {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize generated config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SunriseConfig {
    pub host_name: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    pub unique_id: String,
    pub uuid: String,
    pub mac_address: String,
    #[serde(default)]
    pub server_cert_pem: Option<String>,
    #[serde(default)]
    pub server_private_key_pem: Option<String>,
    #[serde(default)]
    pub paired_clients: Vec<PairedClient>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PairedClient {
    pub unique_id: String,
    pub client_cert_pem: String,
}

impl SunriseConfig {
    pub fn generate() -> Self {
        let uuid = Uuid::new_v4().to_string();
        Self {
            host_name: default_host_name(),
            http_port: DEFAULT_HTTP_PORT,
            https_port: DEFAULT_HTTPS_PORT,
            rtsp_port: DEFAULT_RTSP_PORT,
            unique_id: generate_unique_id(),
            uuid,
            mac_address: generate_mac_address(),
            server_cert_pem: None,
            server_private_key_pem: None,
            paired_clients: Vec::new(),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn write(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let rendered = toml::to_string_pretty(self)?;
        fs::write(path, rendered).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

pub fn load_or_generate(path: impl AsRef<Path>) -> Result<(SunriseConfig, bool), ConfigError> {
    let path = path.as_ref();
    if path.exists() {
        return Ok((SunriseConfig::load(path)?, false));
    }

    let config = SunriseConfig::generate();
    config.write(path)?;
    Ok((config, true))
}

pub fn default_config_path() -> PathBuf {
    PathBuf::from(DEFAULT_CONFIG_FILE)
}

fn default_http_port() -> u16 {
    DEFAULT_HTTP_PORT
}

fn default_https_port() -> u16 {
    DEFAULT_HTTPS_PORT
}

fn default_rtsp_port() -> u16 {
    DEFAULT_RTSP_PORT
}

fn default_host_name() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "sunrise-host".to_string())
}

fn generate_unique_id() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(16)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn generate_mac_address() -> String {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = (bytes[0] & 0b1111_1110) | 0b0000_0010;
    bytes[..6]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_config_on_first_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.toml");

        let (config, created) = load_or_generate(&path).unwrap();

        assert!(created);
        assert!(path.exists());
        assert_eq!(config.http_port, DEFAULT_HTTP_PORT);
        assert_eq!(config.https_port, DEFAULT_HTTPS_PORT);
        assert_eq!(config.rtsp_port, DEFAULT_RTSP_PORT);
        assert!(!config.unique_id.is_empty());
        assert!(!config.uuid.is_empty());
        assert!(!config.mac_address.is_empty());
        assert!(config.server_cert_pem.is_none());
        assert!(config.server_private_key_pem.is_none());
        assert!(config.paired_clients.is_empty());
    }

    #[test]
    fn loads_stable_unique_id_and_uuid_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.toml");
        let config = SunriseConfig {
            host_name: "test-host".to_string(),
            http_port: 47989,
            https_port: 47984,
            rtsp_port: 48010,
            unique_id: "ABCDEF0123456789".to_string(),
            uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            mac_address: "02:AA:BB:CC:DD:EE".to_string(),
            server_cert_pem: Some("cert".to_string()),
            server_private_key_pem: Some("key".to_string()),
            paired_clients: vec![PairedClient {
                unique_id: "client-1".to_string(),
                client_cert_pem: "client-cert".to_string(),
            }],
        };
        config.write(&path).unwrap();

        let (loaded, created) = load_or_generate(&path).unwrap();

        assert!(!created);
        assert_eq!(loaded.unique_id, "ABCDEF0123456789");
        assert_eq!(loaded.uuid, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(loaded.mac_address, "02:AA:BB:CC:DD:EE");
        assert_eq!(loaded.server_cert_pem.as_deref(), Some("cert"));
        assert_eq!(loaded.server_private_key_pem.as_deref(), Some("key"));
        assert_eq!(loaded.paired_clients[0].unique_id, "client-1");
    }

    #[test]
    fn missing_new_fields_default_when_loading_old_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.toml");
        fs::write(
            &path,
            r#"
host_name = "old"
http_port = 47989
https_port = 47984
rtsp_port = 48010
unique_id = "ABCDEF0123456789"
uuid = "550e8400-e29b-41d4-a716-446655440000"
mac_address = "02:AA:BB:CC:DD:EE"
"#,
        )
        .unwrap();

        let loaded = SunriseConfig::load(&path).unwrap();

        assert!(loaded.server_cert_pem.is_none());
        assert!(loaded.server_private_key_pem.is_none());
        assert!(loaded.paired_clients.is_empty());
    }
}
