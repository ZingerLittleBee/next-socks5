//! Server configuration: TOML file loading with CLI overrides.
//!
//! Configuration is loaded from an optional TOML file (via `--config`) and then
//! merged with command-line overrides. When no file is supplied, a sensible
//! default config is used as the base.

use std::path::PathBuf;

/// Top-level server configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Listen address, e.g. `"127.0.0.1:1080"`.
    pub listen: String,
    /// Authentication settings.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Timeout settings.
    #[serde(default)]
    pub timeouts: Timeouts,
    /// Resource limits.
    #[serde(default)]
    pub limits: Limits,
    /// Advertised BND address for UDP ASSOCIATE replies (optional).
    #[serde(default)]
    pub public_addr: Option<String>,
}

/// Authentication configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq, Default)]
pub struct AuthConfig {
    /// Authentication method to require from clients.
    #[serde(default)]
    pub method: AuthMethod,
    /// Username/password credentials (used when `method` is `Password`).
    #[serde(default)]
    pub users: Vec<User>,
}

/// Supported authentication methods.
#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// No authentication required.
    #[default]
    None,
    /// Username/password authentication (RFC 1929).
    Password,
}

/// A username/password credential pair.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct User {
    /// Username.
    pub username: String,
    /// Password.
    pub password: String,
}

/// Timeout settings, all in milliseconds.
#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
pub struct Timeouts {
    /// Timeout for establishing the upstream connection.
    pub connect_ms: u64,
    /// Idle timeout for TCP relays.
    pub tcp_idle_ms: u64,
    /// Idle timeout for UDP associations.
    pub udp_idle_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_ms: 10_000,
            tcp_idle_ms: 300_000,
            udp_idle_ms: 60_000,
        }
    }
}

/// Resource limits.
#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq, Default)]
pub struct Limits {
    /// Maximum number of concurrent connections (unbounded when `None`).
    #[serde(default)]
    pub max_connections: Option<usize>,
}

/// Command-line arguments.
#[derive(Debug, clap::Parser)]
#[command(name = "next-socks5", about = "A lightweight SOCKS5 server")]
pub struct Cli {
    /// Path to a TOML configuration file.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Override the listen address from the config file.
    #[arg(long)]
    pub listen: Option<String>,
    /// Disable the terminal UI.
    #[arg(long)]
    pub no_tui: bool,
}

/// Errors that can occur while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Failed to read the config file.
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    /// Failed to parse the config file as TOML.
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
}

impl Config {
    /// Load configuration from the CLI options.
    ///
    /// If `cli.config` is `Some`, the referenced TOML file is read and parsed;
    /// otherwise a default config is used as the base. CLI overrides are then
    /// applied on top.
    pub fn load(cli: &Cli) -> Result<Config, ConfigError> {
        let mut config = match &cli.config {
            Some(path) => {
                let contents = std::fs::read_to_string(path)?;
                Self::from_toml_str(&contents)?
            }
            None => Self::default_config(),
        };
        apply_overrides(&mut config, cli);
        Ok(config)
    }

    /// Parse a [`Config`] from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Config, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    /// Build the default config: listen on `127.0.0.1:1080` with defaults.
    fn default_config() -> Config {
        Config {
            listen: "127.0.0.1:1080".to_string(),
            auth: AuthConfig::default(),
            timeouts: Timeouts::default(),
            limits: Limits::default(),
            public_addr: None,
        }
    }
}

/// Apply CLI overrides onto a parsed/default config in place.
///
/// Kept as a pure helper so the merge logic is unit-testable without touching
/// the filesystem.
fn apply_overrides(cfg: &mut Config, cli: &Cli) {
    if let Some(listen) = &cli.listen {
        cfg.listen = listen.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_CONFIG: &str = r#"
listen = "127.0.0.1:1080"

[auth]
method = "password"
[[auth.users]]
username = "alice"
password = "secret"

[timeouts]
connect_ms = 10000
tcp_idle_ms = 300000
udp_idle_ms = 60000

[limits]
max_connections = 1024
"#;

    #[test]
    fn parses_example_config() {
        let cfg = Config::from_toml_str(EXAMPLE_CONFIG).expect("should parse");
        assert_eq!(cfg.listen, "127.0.0.1:1080");
        assert_eq!(cfg.auth.method, AuthMethod::Password);
        assert_eq!(
            cfg.auth.users,
            vec![User {
                username: "alice".to_string(),
                password: "secret".to_string(),
            }]
        );
        assert_eq!(cfg.timeouts.connect_ms, 10_000);
        assert_eq!(cfg.timeouts.tcp_idle_ms, 300_000);
        assert_eq!(cfg.timeouts.udp_idle_ms, 60_000);
        assert_eq!(cfg.limits.max_connections, Some(1024));
    }

    #[test]
    fn defaults_fill_when_sections_omitted() {
        let cfg = Config::from_toml_str(r#"listen = "0.0.0.0:1080""#).expect("should parse");
        assert_eq!(cfg.listen, "0.0.0.0:1080");
        assert_eq!(cfg.auth.method, AuthMethod::None);
        assert!(cfg.auth.users.is_empty());
        assert_eq!(cfg.timeouts, Timeouts::default());
        assert_eq!(cfg.limits.max_connections, None);
        assert_eq!(cfg.public_addr, None);
    }

    #[test]
    fn cli_listen_override_replaces_listen() {
        let mut cfg = Config::from_toml_str(EXAMPLE_CONFIG).expect("should parse");
        let cli = Cli {
            config: None,
            listen: Some("0.0.0.0:9999".to_string()),
            no_tui: false,
        };
        apply_overrides(&mut cfg, &cli);
        assert_eq!(cfg.listen, "0.0.0.0:9999");
    }

    #[test]
    fn cli_override_absent_keeps_listen() {
        let mut cfg = Config::from_toml_str(EXAMPLE_CONFIG).expect("should parse");
        let cli = Cli {
            config: None,
            listen: None,
            no_tui: false,
        };
        apply_overrides(&mut cfg, &cli);
        assert_eq!(cfg.listen, "127.0.0.1:1080");
    }

    #[test]
    fn auth_method_serde_rename() {
        let pw = Config::from_toml_str("listen = \"x\"\n[auth]\nmethod = \"password\"")
            .expect("should parse");
        assert_eq!(pw.auth.method, AuthMethod::Password);

        let none = Config::from_toml_str("listen = \"x\"\n[auth]\nmethod = \"none\"")
            .expect("should parse");
        assert_eq!(none.auth.method, AuthMethod::None);
    }
}
