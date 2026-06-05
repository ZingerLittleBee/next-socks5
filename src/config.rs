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
    /// UDP relay transport/addressing configuration.
    #[serde(default)]
    pub udp: UdpConfig,
    /// Admin/attach endpoint settings.
    #[serde(default)]
    pub admin: AdminConfig,
    /// Egress (destination) policy guarding against SSRF / open-relay abuse.
    #[serde(default)]
    pub egress: Egress,
}

/// Admin/attach (local Unix socket) configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct AdminConfig {
    /// Whether the admin endpoint is enabled (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the admin socket path (default `/run/next-socks5/admin.sock`).
    #[serde(default)]
    pub socket: Option<String>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket: None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Default admin socket path when none configured.
pub const DEFAULT_ADMIN_SOCKET: &str = "/run/next-socks5/admin.sock";

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
    /// Deadline for the whole pre-relay phase (greeting, auth, request). Bounds
    /// slow/stalled clients so a half-open handshake cannot pin a task forever.
    #[serde(default = "default_handshake_ms")]
    pub handshake_ms: u64,
    /// Timeout for establishing the upstream connection (also bounds DNS).
    pub connect_ms: u64,
    /// Idle timeout for TCP relays.
    pub tcp_idle_ms: u64,
    /// Idle timeout for UDP associations.
    pub udp_idle_ms: u64,
}

/// Default pre-relay handshake/auth/request deadline.
fn default_handshake_ms() -> u64 {
    10_000
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake_ms: default_handshake_ms(),
            connect_ms: 10_000,
            tcp_idle_ms: 300_000,
            udp_idle_ms: 60_000,
        }
    }
}

/// An inclusive UDP relay port range `[start, end]` for binding association
/// sockets. `start == end` is a single fixed port. Deserialized from a
/// `"start-end"` string (e.g. `"40000-40100"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    /// First port in the range (inclusive); must be >= 1.
    pub start: u16,
    /// Last port in the range (inclusive); must be >= `start`.
    pub end: u16,
}

impl PortRange {
    /// Parse an inclusive `"start-end"` range. Rejects a missing dash,
    /// non-numeric bounds, port 0 as `start`, and `start > end`.
    fn parse(s: &str) -> Result<PortRange, String> {
        let (start, end) = s
            .split_once('-')
            .ok_or_else(|| format!("expected 'start-end', got {s:?}"))?;
        let start: u16 = start
            .trim()
            .parse()
            .map_err(|_| format!("invalid start port {start:?}"))?;
        let end: u16 = end
            .trim()
            .parse()
            .map_err(|_| format!("invalid end port {end:?}"))?;
        if start == 0 {
            return Err("start port must be >= 1".to_string());
        }
        if start > end {
            return Err(format!("start {start} must be <= end {end}"));
        }
        Ok(PortRange { start, end })
    }
}

impl<'de> serde::Deserialize<'de> for PortRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        PortRange::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// UDP relay transport/addressing configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq, Default)]
pub struct UdpConfig {
    /// Bind each association's relay socket inside this inclusive port range.
    /// `None` => OS-assigned ephemeral port.
    #[serde(default)]
    pub port_range: Option<PortRange>,
    /// Advertised BND.ADDR IP for UDP ASSOCIATE replies (advertise-only; the
    /// advertised port is always the real bound port). `None` => advertise the
    /// bound address. Needed behind NAT/Docker. Accepts a bare IP or an
    /// `ip:port` (the port is ignored); a malformed value is rejected at config
    /// load so a typo fails fast instead of being silently ignored at runtime.
    #[serde(default, deserialize_with = "de_advertise_ip")]
    pub advertise: Option<std::net::IpAddr>,
}

/// Deserialize `[udp].advertise`: accept a bare IP or an `ip:port` (the port is
/// ignored — only the IP is advertised), rejecting anything else at config load.
fn de_advertise_ip<'de, D>(deserializer: D) -> Result<Option<std::net::IpAddr>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = <String as serde::Deserialize>::deserialize(deserializer)?;
    parse_advertise_ip(&s)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// Parse a `[udp].advertise` value into an IP. Accepts a bare IP, or an
/// `ip:port` whose port is discarded; returns an error message otherwise.
fn parse_advertise_ip(s: &str) -> Result<std::net::IpAddr, String> {
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Ok(ip);
    }
    if let Ok(sa) = s.parse::<std::net::SocketAddr>() {
        return Ok(sa.ip());
    }
    Err(format!(
        "invalid advertise address {s:?}: expected an IP (e.g. \"203.0.113.42\") or \"ip:port\""
    ))
}

/// Resource limits.
#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
pub struct Limits {
    /// Maximum number of concurrent connections (unbounded when `None`).
    /// Enforced at accept time, counting half-open/handshaking connections too.
    #[serde(default)]
    pub max_connections: Option<usize>,
    /// Maximum concurrent connections from a single source IP (unbounded when
    /// `None`). Enforced at accept time alongside `max_connections`.
    #[serde(default)]
    pub max_per_ip: Option<usize>,
    /// Maximum distinct targets tracked per UDP association before the oldest is
    /// evicted, bounding per-association memory.
    #[serde(default = "default_udp_max_targets")]
    pub udp_max_targets: usize,
    /// Optional cap on outbound datagrams per second per UDP association,
    /// limiting reflection/flood abuse. `None` leaves it unlimited.
    #[serde(default)]
    pub udp_rate_pps: Option<u32>,
}

/// Default per-association distinct-target cap for UDP relays.
fn default_udp_max_targets() -> usize {
    1024
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: None,
            max_per_ip: None,
            udp_max_targets: default_udp_max_targets(),
            udp_rate_pps: None,
        }
    }
}

/// Egress (destination) policy. Blocks SOCKS targets that resolve to
/// internal/metadata addresses, mitigating SSRF and open-relay abuse. Each
/// class can be individually disabled for trusted deployments (e.g. an internal
/// LAN proxy). Secure by default: all classes blocked.
#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
pub struct Egress {
    /// Block loopback (127.0.0.0/8, ::1).
    #[serde(default = "default_true")]
    pub block_loopback: bool,
    /// Block link-local (169.254.0.0/16 incl. cloud metadata, fe80::/10).
    #[serde(default = "default_true")]
    pub block_link_local: bool,
    /// Block private ranges (10/8, 172.16/12, 192.168/16, fc00::/7).
    #[serde(default = "default_true")]
    pub block_private: bool,
}

impl Default for Egress {
    fn default() -> Self {
        Self {
            block_loopback: true,
            block_link_local: true,
            block_private: true,
        }
    }
}

impl Egress {
    /// A permissive policy that allows every destination (used by tests and by
    /// operators who explicitly opt out of egress filtering).
    pub fn permissive() -> Self {
        Self {
            block_loopback: false,
            block_link_local: false,
            block_private: false,
        }
    }

    /// Whether a resolved destination IP is blocked by this policy.
    ///
    /// The unspecified address (`0.0.0.0` / `::`) is always blocked. Native IPv6
    /// classes are evaluated first; any IPv4-in-IPv6 form (`::ffff:a.b.c.d`
    /// mapped or `::a.b.c.d` compatible) is then folded down to IPv4 so it cannot
    /// bypass the v4 rules. `block_loopback` also covers `0.0.0.0/8` ("this
    /// network", which several stacks route to localhost); `block_private` also
    /// covers CGNAT `100.64.0.0/10`.
    pub fn is_blocked(&self, ip: std::net::IpAddr) -> bool {
        use std::net::IpAddr;
        // Honor native IPv6 classes first, then fold any IPv4-in-IPv6 form down
        // to IPv4 for the v4 rules.
        let ip = match ip {
            IpAddr::V6(v6) => {
                if v6.is_unspecified()
                    || (self.block_loopback && v6.is_loopback())
                    || (self.block_link_local && (v6.segments()[0] & 0xffc0) == 0xfe80)
                    || (self.block_private && (v6.segments()[0] & 0xfe00) == 0xfc00)
                {
                    return true;
                }
                match v6.to_ipv4() {
                    Some(v4) => IpAddr::V4(v4),
                    // A global/other IPv6 address: allowed.
                    None => return false,
                }
            }
            v4 => v4,
        };
        match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                v4.is_unspecified()
                    || (self.block_loopback && (v4.is_loopback() || o[0] == 0))
                    || (self.block_link_local && v4.is_link_local())
                    || (self.block_private
                        && (v4.is_private() || (o[0] == 100 && (o[1] & 0xc0) == 64)))
            }
            // Unreachable: every IPv6 path above returned or normalized to IPv4.
            IpAddr::V6(_) => false,
        }
    }
}

/// Command-line arguments.
#[derive(Debug, clap::Parser)]
#[command(name = "next-socks5", about = "A lightweight SOCKS5 server")]
pub struct Cli {
    /// Subcommand. `serve` runs the server; with no subcommand, usage is shown.
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Path to a TOML configuration file.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Override the listen address from the config file.
    #[arg(long, global = true)]
    pub listen: Option<String>,
    /// Disable the terminal UI.
    #[arg(long, global = true)]
    pub no_tui: bool,
    /// Disable the local admin/attach endpoint.
    #[arg(long, global = true)]
    pub no_admin: bool,
    /// Override the admin socket path.
    #[arg(long, global = true)]
    pub admin_socket: Option<PathBuf>,
    /// Feed the dashboard with synthetic data (demo only; no real traffic).
    #[arg(long, hide = true, global = true)]
    pub mock: bool,
}

/// Subcommands. With no subcommand, a bare invocation prints usage; `serve`
/// (alias `run`) starts the server.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Run the SOCKS5 server.
    #[command(visible_alias = "run")]
    Serve,
    /// Attach to a running server and show its dashboard.
    Attach {
        /// Path to the admin socket to connect to.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
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
            udp: UdpConfig::default(),
            admin: AdminConfig::default(),
            egress: Egress::default(),
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
    if cli.no_admin {
        cfg.admin.enabled = false;
    }
    if let Some(sock) = &cli.admin_socket {
        cfg.admin.socket = Some(sock.to_string_lossy().into_owned());
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
        assert_eq!(cfg.udp, UdpConfig::default());
    }

    #[test]
    fn cli_listen_override_replaces_listen() {
        let mut cfg = Config::from_toml_str(EXAMPLE_CONFIG).expect("should parse");
        let cli = Cli {
            command: None,
            config: None,
            listen: Some("0.0.0.0:9999".to_string()),
            no_tui: false,
            no_admin: false,
            admin_socket: None,
            mock: false,
        };
        apply_overrides(&mut cfg, &cli);
        assert_eq!(cfg.listen, "0.0.0.0:9999");
    }

    #[test]
    fn cli_override_absent_keeps_listen() {
        let mut cfg = Config::from_toml_str(EXAMPLE_CONFIG).expect("should parse");
        let cli = Cli {
            command: None,
            config: None,
            listen: None,
            no_tui: false,
            no_admin: false,
            admin_socket: None,
            mock: false,
        };
        apply_overrides(&mut cfg, &cli);
        assert_eq!(cfg.listen, "127.0.0.1:1080");
    }

    #[test]
    fn parses_admin_config() {
        let cfg = Config::from_toml_str(
            "listen = \"x\"\n[admin]\nenabled = false\nsocket = \"/tmp/a.sock\"",
        )
        .expect("should parse");
        assert!(!cfg.admin.enabled);
        assert_eq!(cfg.admin.socket.as_deref(), Some("/tmp/a.sock"));
    }

    #[test]
    fn admin_defaults_enabled() {
        let cfg = Config::from_toml_str("listen = \"x\"").expect("should parse");
        assert!(cfg.admin.enabled);
        assert_eq!(cfg.admin.socket, None);
    }

    #[test]
    fn cli_admin_socket_override() {
        let mut cfg = Config::from_toml_str("listen = \"x\"").unwrap();
        let cli = Cli {
            command: None,
            config: None,
            listen: None,
            no_tui: false,
            no_admin: true,
            admin_socket: Some(PathBuf::from("/run/x.sock")),
            mock: false,
        };
        apply_overrides(&mut cfg, &cli);
        assert!(!cfg.admin.enabled);
        assert_eq!(cfg.admin.socket.as_deref(), Some("/run/x.sock"));
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

    #[test]
    fn egress_default_blocks_internal_destinations() {
        use std::net::IpAddr;
        let e = Egress::default();
        // Loopback, link-local (incl. cloud metadata), private, CGNAT, 0/8.
        for s in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(e.is_blocked(ip), "{s} should be blocked by default");
        }
    }

    #[test]
    fn egress_default_allows_public_destinations() {
        use std::net::IpAddr;
        let e = Egress::default();
        for s in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2001:4860:4860::8888"] {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!e.is_blocked(ip), "{s} should be allowed by default");
        }
    }

    #[test]
    fn egress_permissive_allows_loopback() {
        use std::net::IpAddr;
        let e = Egress::permissive();
        assert!(!e.is_blocked("127.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(!e.is_blocked("10.0.0.1".parse::<IpAddr>().unwrap()));
        // The unspecified address is blocked regardless of policy.
        assert!(e.is_blocked("0.0.0.0".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn port_range_parses_valid() {
        assert_eq!(
            PortRange::parse("40000-40100"),
            Ok(PortRange { start: 40000, end: 40100 })
        );
    }

    #[test]
    fn port_range_allows_single_port() {
        assert_eq!(
            PortRange::parse("40000-40000"),
            Ok(PortRange { start: 40000, end: 40000 })
        );
    }

    #[test]
    fn port_range_rejects_malformed() {
        assert!(PortRange::parse("5000").is_err()); // no dash
        assert!(PortRange::parse("a-b").is_err()); // non-numeric
        assert!(PortRange::parse("5000-4000").is_err()); // start > end
        assert!(PortRange::parse("0-100").is_err()); // start port 0
    }

    #[test]
    fn parses_udp_section() {
        let cfg = Config::from_toml_str(
            "listen = \"x\"\n[udp]\nport_range = \"40000-40100\"\nadvertise = \"203.0.113.42\"",
        )
        .expect("should parse");
        assert_eq!(
            cfg.udp.port_range,
            Some(PortRange { start: 40000, end: 40100 })
        );
        assert_eq!(cfg.udp.advertise, Some("203.0.113.42".parse().unwrap()));
    }

    #[test]
    fn udp_section_defaults_empty() {
        let cfg = Config::from_toml_str("listen = \"x\"").expect("should parse");
        assert_eq!(cfg.udp, UdpConfig::default());
    }

    #[test]
    fn advertise_rejects_malformed() {
        // A typo'd advertise address must fail at config load, not be silently
        // ignored at runtime.
        let res = Config::from_toml_str("listen = \"x\"\n[udp]\nadvertise = \"not-an-ip\"");
        assert!(res.is_err(), "malformed advertise must be rejected at load");
    }

    #[test]
    fn advertise_accepts_ip_port() {
        // An `ip:port` form is accepted; only the IP is kept (port ignored).
        let cfg = Config::from_toml_str("listen = \"x\"\n[udp]\nadvertise = \"203.0.113.42:1080\"")
            .expect("ip:port advertise should parse");
        assert_eq!(cfg.udp.advertise, Some("203.0.113.42".parse().unwrap()));
    }
}
