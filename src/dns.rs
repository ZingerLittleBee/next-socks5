//! Shared asynchronous DNS resolution with independent address-family errors.

use std::io;
use std::net::{IpAddr, SocketAddr};

use hickory_resolver::config::{LookupIpStrategy, NameServerConfig, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::TokioResolver;
use tokio::time::{timeout_at, Instant};

use crate::config::DnsConfig;

/// Bounded, TTL-aware caches keep errors for one address family independent.
pub struct DnsResolver {
    ipv4: TokioResolver,
    ipv6: TokioResolver,
}

/// The target, elapsed time, and underlying DNS failure.
#[derive(Debug, thiserror::Error)]
#[error("DNS lookup for {host} failed after {elapsed_ms} ms: {reason}")]
pub struct ResolveError {
    host: String,
    elapsed_ms: u128,
    reason: String,
}

impl DnsResolver {
    /// Load system DNS unless explicit upstreams were supplied. No public DNS
    /// service is substituted when configuration or a lookup fails.
    pub fn new(config: &DnsConfig) -> io::Result<Self> {
        let build = |strategy| {
            let mut builder = if config.servers.is_empty() {
                TokioResolver::builder_tokio().map_err(io::Error::other)?
            } else {
                let mut upstreams = ResolverConfig::from_name_servers(Vec::new());
                for address in &config.servers {
                    let mut server = NameServerConfig::udp_and_tcp(address.ip());
                    for connection in &mut server.connections {
                        connection.port = address.port();
                    }
                    upstreams.add_name_server(server);
                }
                TokioResolver::builder_with_config(upstreams, TokioRuntimeProvider::default())
            };
            let options = builder.options_mut();
            options.ip_strategy = strategy;
            options.use_hosts_file = ResolveHosts::Always;
            options.cache_size = 128;
            builder.build().map_err(io::Error::other)
        };
        Ok(Self {
            ipv4: build(LookupIpStrategy::Ipv4Only)?,
            ipv6: build(LookupIpStrategy::Ipv6Only)?,
        })
    }

    /// Query only the requested family, with a caller-owned deadline.
    pub async fn lookup(
        &self,
        host: &str,
        port: u16,
        deadline: Instant,
        ipv4: bool,
    ) -> Result<Vec<SocketAddr>, ResolveError> {
        let started = Instant::now();
        let resolver = if ipv4 { &self.ipv4 } else { &self.ipv6 };
        query(resolver, host, deadline)
            .await
            .map(|addresses| {
                addresses
                    .into_iter()
                    .map(|ip| SocketAddr::new(ip, port))
                    .collect()
            })
            .map_err(|reason| ResolveError {
                host: host.to_owned(),
                elapsed_ms: started.elapsed().as_millis(),
                reason,
            })
    }
}

async fn query(
    resolver: &TokioResolver,
    host: &str,
    deadline: Instant,
) -> Result<Vec<IpAddr>, String> {
    let lookup = timeout_at(deadline, resolver.lookup_ip(host))
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|error| {
            if let hickory_resolver::net::NetError::Dns(
                hickory_resolver::net::DnsError::NoRecordsFound(records),
            ) = &error
            {
                let code = records.response_code;
                if code == hickory_resolver::proto::op::ResponseCode::NoError {
                    return "no addresses returned (NOERROR)".to_string();
                }
                return format!("{code}: {error}");
            }
            error.to_string()
        })?;
    let mut addresses = Vec::new();
    for ip in lookup.iter() {
        let ip = ip.to_canonical();
        if !addresses.contains(&ip) {
            addresses.push(ip);
        }
    }
    if addresses.is_empty() {
        Err("no addresses returned".into())
    } else {
        Ok(addresses)
    }
}
