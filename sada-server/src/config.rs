//! Configuration schema and loading.

use std::{
    env,
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use thiserror::Error;

/// Environment variable naming the configuration file.
const CONFIG_ENV: &str = "SADA_CONFIG";

/// Configuration file used when [`CONFIG_ENV`] is unset.
const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// How long an auth code lives when the configuration does not say.
const DEFAULT_CODE_TTL_SECONDS: u64 = 300;

/// Result type used by configuration loading.
pub type Result<T> = std::result::Result<T, Error>;

/// Complete server configuration loaded from TOML.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Listener settings.
    pub server: ServerConfig,
    /// WebRTC transport settings.
    #[serde(default)]
    pub webrtc: WebRtcConfig,
    /// Player authentication settings.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Audio routing settings.
    #[serde(default)]
    pub routing: RoutingConfig,
}

/// Audio routing settings.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// How listener sets are decided.
    pub policy: RoutingPolicy,
}

/// How the server decides who hears whom.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingPolicy {
    /// Everyone hears everyone. Works with no game server attached.
    #[default]
    Broadcast,
    /// Relay along the hearer lists the game computes.
    HearerList,
    /// Decide from raw coordinates and a distance threshold.
    Proximity,
}

/// Listener configuration.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// Socket address the HTTP and WebSocket server listens on.
    pub listen: SocketAddr,
    /// Unix socket path used by the BYOND bridge control channel.
    ///
    /// When absent the control channel is not started.
    pub control_socket: Option<PathBuf>,
}

/// WebRTC transport configuration.
#[derive(Default, Debug, Deserialize)]
#[serde(default)]
pub struct WebRtcConfig {
    /// IP address advertised as the host ICE candidate.
    ///
    /// When absent the first usable non-loopback IPv4 interface is used.
    pub host_ip: Option<IpAddr>,
}

/// Player authentication configuration.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Accept sessions that present no auth code, binding them to no player.
    pub allow_anonymous: bool,
    /// How long a code the game mints stays valid, in seconds.
    pub code_ttl_seconds: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            allow_anonymous: true,
            code_ttl_seconds: DEFAULT_CODE_TTL_SECONDS,
        }
    }
}

impl AuthConfig {
    /// [`code_ttl_seconds`](Self::code_ttl_seconds) as a duration.
    pub fn code_ttl(&self) -> Duration { Duration::from_secs(self.code_ttl_seconds) }
}

impl Config {
    /// Load configuration from a TOML file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|s| Error::Read(path.to_owned(), s))?;
        let config = toml::from_str(&content).map_err(|s| Error::Parse(path.to_owned(), s))?;
        Ok(config)
    }

    /// Load configuration from the environment variable [`CONFIG_ENV`] or the default path [`DEFAULT_CONFIG_PATH`].
    pub fn load_from_env() -> Result<Self> {
        let path = env::var(CONFIG_ENV).map_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
        let config = Self::load(&path);
        info!(path = %path.display(), "configuration loaded");
        config
    }
}

/// Errors that can happen while loading server configuration.
#[derive(Debug, Error)]
pub enum Error {
    /// The configuration file could not be read.
    #[error("failed to read config from {0}")]
    Read(PathBuf, #[source] std::io::Error),
    /// The configuration file was not valid TOML for the expected schema.
    #[error("failed to parse config from {0}")]
    Parse(PathBuf, #[source] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Config, RoutingPolicy};

    #[test]
    fn optional_sections_default() {
        let config: Config = toml::from_str("[server]\nlisten = \"0.0.0.0:3000\"\n").unwrap();
        assert_eq!(config.server.listen.port(), 3000);
        assert!(config.server.control_socket.is_none());
        assert!(config.webrtc.host_ip.is_none());
        assert!(config.auth.allow_anonymous);
        assert_eq!(config.auth.code_ttl(), Duration::from_secs(300));
        assert_eq!(config.routing.policy, RoutingPolicy::Broadcast);
    }

    #[test]
    fn the_routing_policy_is_named_in_kebab_case() {
        let config: Config =
            toml::from_str("[server]\nlisten = \"0.0.0.0:3000\"\n\n[routing]\npolicy = \"hearer-list\"\n").unwrap();
        assert_eq!(config.routing.policy, RoutingPolicy::HearerList);
    }
}
