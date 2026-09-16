//! DNS regressions exercised through real SOCKS connections and a local DNS server.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use next_socks5::config::{Config, Egress};
use next_socks5::metrics::{Event, Metrics};
use next_socks5::protocol::address::Address;
use next_socks5::protocol::udp::{decap, encap};
use next_socks5::server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const HOST: &str = "dns-regression.test";
const IO_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
enum Answer {
    Addresses(Vec<IpAddr>),
    ServFail,
    NxDomain,
    Silent,
    Delayed(Duration, Box<Answer>),
}

fn v4() -> Answer {
    Answer::Addresses(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
}

fn v6() -> Answer {
    Answer::Addresses(vec![IpAddr::V6(Ipv6Addr::LOCALHOST)])
}

fn empty() -> Answer {
    Answer::Addresses(Vec::new())
}

struct DnsFixture {
    address: SocketAddr,
    task: JoinHandle<()>,
    queries: Arc<AtomicU8>,
    query_count: Arc<AtomicUsize>,
}

impl Drop for DnsFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DnsFixture {
    async fn start(a: Answer, aaaa: Answer) -> Self {
        Self::with_ttl(a, aaaa, 60).await
    }

    async fn with_ttl(a: Answer, aaaa: Answer, ttl: u32) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let socket = Arc::new(socket);
        let queries = Arc::new(AtomicU8::new(0));
        let observed = queries.clone();
        let query_count = Arc::new(AtomicUsize::new(0));
        let counted = query_count.clone();
        let task = tokio::spawn(async move {
            let mut buffer = [0u8; 2048];
            loop {
                let (length, peer) = socket.recv_from(&mut buffer).await.unwrap();
                counted.fetch_add(1, Ordering::SeqCst);
                let query = &buffer[..length];
                assert!(
                    query.len() >= 17,
                    "DNS question must contain a header and name"
                );
                let mut end = 12;
                while query[end] != 0 {
                    end += usize::from(query[end]) + 1;
                }
                end += 1;
                let qtype = u16::from_be_bytes([query[end], query[end + 1]]);
                observed.fetch_or(if qtype == 1 { 1 } else { 2 }, Ordering::SeqCst);
                let answer = match qtype {
                    1 => &a,
                    28 => &aaaa,
                    _ => panic!("unexpected DNS query type {qtype}"),
                };
                let (answer, delay) = match answer {
                    Answer::Delayed(delay, answer) => (answer.as_ref(), *delay),
                    answer => (answer, Duration::ZERO),
                };
                if matches!(answer, Answer::Silent) {
                    continue;
                }
                let rcode = match answer {
                    Answer::ServFail => 2,
                    Answer::NxDomain => 3,
                    _ => 0,
                };
                let records = match answer {
                    Answer::Addresses(ips) => ips.as_slice(),
                    _ => &[],
                };
                let mut response = Vec::new();
                response.extend_from_slice(&query[..2]);
                response.extend_from_slice(&(0x8180u16 | rcode).to_be_bytes());
                response.extend_from_slice(&1u16.to_be_bytes());
                response.extend_from_slice(&(records.len() as u16).to_be_bytes());
                response.extend_from_slice(&[0; 4]);
                response.extend_from_slice(&query[12..end + 4]);
                for ip in records {
                    response.extend_from_slice(&[0xc0, 0x0c]);
                    response.extend_from_slice(&qtype.to_be_bytes());
                    response.extend_from_slice(&1u16.to_be_bytes());
                    response.extend_from_slice(&ttl.to_be_bytes());
                    match ip {
                        IpAddr::V4(ip) => {
                            assert_eq!(qtype, 1);
                            response.extend_from_slice(&4u16.to_be_bytes());
                            response.extend_from_slice(&ip.octets());
                        }
                        IpAddr::V6(ip) => {
                            assert_eq!(qtype, 28);
                            response.extend_from_slice(&16u16.to_be_bytes());
                            response.extend_from_slice(&ip.octets());
                        }
                    }
                }
                let socket = socket.clone();
                tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    socket.send_to(&response, peer).await.unwrap();
                });
            }
        });
        Self {
            address,
            task,
            queries,
            query_count,
        }
    }

    async fn wait_for_queries(&self, families: u8) {
        timeout(IO_TIMEOUT, async {
            while self.queries.load(Ordering::SeqCst) & families != families {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("proxy must query the required address families on the configured DNS server");
    }

    fn config(&self, ipv6: bool) -> Config {
        let listen = if ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
        let mut config = Config::from_toml_str(&format!(
            "listen = \"{listen}\"\n[dns]\nservers = [\"{}\"]\n",
            self.address
        ))
        .unwrap();
        config.egress = Egress::permissive();
        config.timeouts.connect_ms = 1000;
        config
    }
}

struct Proxy {
    address: SocketAddr,
    events: broadcast::Receiver<Event>,
    shutdown: watch::Sender<bool>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

impl Proxy {
    async fn start(config: Config) -> Self {
        let listener = TcpListener::bind(&config.listen).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, receiver) = broadcast::channel(64);
        let (shutdown, shutdown_rx) = watch::channel(false);
        tokio::spawn(server::run(
            listener,
            Arc::new(config),
            Metrics::new(),
            events,
            shutdown_rx,
        ));
        Self {
            address,
            events: receiver,
            shutdown,
        }
    }

    async fn request(&self, command: u8, target: Address) -> (TcpStream, u8, Address) {
        timeout(IO_TIMEOUT, async {
            let mut stream = TcpStream::connect(self.address).await.unwrap();
            stream.write_all(&[5, 1, 0]).await.unwrap();
            let mut handshake = [0; 2];
            stream.read_exact(&mut handshake).await.unwrap();
            assert_eq!(handshake, [5, 0]);
            let mut request = vec![5, command, 0];
            target.encode(&mut request);
            stream.write_all(&request).await.unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], 5);
            let length = match header[3] {
                1 => 6,
                4 => 18,
                other => panic!("unexpected reply address type {other}"),
            };
            let mut address = vec![0; length + 1];
            address[0] = header[3];
            stream.read_exact(&mut address[1..]).await.unwrap();
            (stream, header[1], Address::decode(&address).unwrap().0)
        })
        .await
        .expect("SOCKS request exceeded the test deadline")
    }

    async fn error(&mut self) -> String {
        timeout(IO_TIMEOUT, async {
            loop {
                if let Event::Error { msg, .. } = self.events.recv().await.unwrap() {
                    return msg;
                }
            }
        })
        .await
        .expect("expected an error event")
    }
}

async fn tcp_echo(a: Answer, aaaa: Answer, ipv6: bool) {
    tcp_echo_with_egress(a, aaaa, ipv6, Egress::permissive()).await;
}

async fn tcp_echo_with_egress(a: Answer, aaaa: Answer, ipv6: bool, egress: Egress) {
    let dns = DnsFixture::start(a, aaaa).await;
    let mut config = dns.config(false);
    config.egress = egress;
    let proxy = Proxy::start(config).await;
    let listener = TcpListener::bind(if ipv6 { "[::1]:0" } else { "127.0.0.1:0" })
        .await
        .unwrap();
    let target_port = listener.local_addr().unwrap().port();
    let echo = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4];
        stream.read_exact(&mut buffer).await.unwrap();
        stream.write_all(&buffer).await.unwrap();
    });
    let (mut stream, code, _) = proxy
        .request(1, Address::Domain(HOST.to_owned(), target_port))
        .await;
    assert_eq!(
        code, 0,
        "DNS must retain a usable address from either family"
    );
    dns.wait_for_queries(3).await;
    timeout(IO_TIMEOUT, async {
        stream.write_all(b"ping").await.unwrap();
        let mut received = [0; 4];
        stream.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"ping");
        echo.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tcp_a_survives_aaaa_servfail() {
    tcp_echo(v4(), Answer::ServFail, false).await;
}

#[tokio::test]
async fn tcp_a_survives_aaaa_nxdomain() {
    tcp_echo(v4(), Answer::NxDomain, false).await;
}

#[tokio::test]
async fn tcp_ipv4_only_nodata_succeeds() {
    tcp_echo(v4(), empty(), false).await;
}

#[tokio::test]
async fn tcp_ipv6_only_nodata_succeeds() {
    tcp_echo(empty(), v6(), true).await;
}

#[tokio::test]
async fn tcp_dual_stack_succeeds() {
    tcp_echo(v4(), v6(), false).await;
}

#[tokio::test]
async fn tcp_tries_next_address_after_connection_refused() {
    tcp_echo(
        Answer::Addresses(vec![
            "127.0.0.2".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ]),
        empty(),
        false,
    )
    .await;
}

#[tokio::test]
async fn tcp_dns_error_preserves_resolver_detail() {
    let dns = DnsFixture::start(Answer::NxDomain, Answer::NxDomain).await;
    let mut proxy = Proxy::start(dns.config(false)).await;
    let (_, code, _) = proxy
        .request(1, Address::Domain(HOST.to_owned(), 443))
        .await;
    assert_eq!(code, 4);
    let message = proxy.error().await;
    assert!(message.contains(HOST), "missing DNS target: {message}");
    let detail = message.to_ascii_lowercase();
    assert!(
        detail.contains("nxdomain") || detail.contains("non-existent domain"),
        "missing resolver failure detail: {message}"
    );
    assert!(detail.contains("ms"), "missing elapsed duration: {message}");
}

#[tokio::test]
async fn tcp_dns_timeout_is_distinct_from_lookup_failure() {
    let dns = DnsFixture::start(Answer::Silent, Answer::Silent).await;
    let mut config = dns.config(false);
    config.timeouts.connect_ms = 100;
    let mut proxy = Proxy::start(config).await;
    let (_, code, _) = proxy
        .request(1, Address::Domain(HOST.to_owned(), 443))
        .await;
    assert_eq!(code, 4);
    let message = proxy.error().await;
    assert!(message.contains(HOST), "missing DNS target: {message}");
    assert!(
        message.contains("timed out") || message.contains("timeout"),
        "missing timeout diagnosis: {message}"
    );
    assert!(
        message.contains("ms"),
        "missing elapsed duration: {message}"
    );
}

#[tokio::test]
async fn tcp_dns_answers_obey_egress_policy() {
    for ip in ["127.0.0.1", "10.0.0.1"] {
        let dns = DnsFixture::start(Answer::Addresses(vec![ip.parse().unwrap()]), empty()).await;
        let mut config = dns.config(false);
        config.egress = Egress::default();
        let proxy = Proxy::start(config).await;
        let (_, code, _) = proxy.request(1, Address::Domain(HOST.to_owned(), 9)).await;
        assert_eq!(code, 2, "DNS must not bypass the egress policy for {ip}");
    }
}

fn socket_address(address: Address) -> SocketAddr {
    match address {
        Address::V4(ip, port) => SocketAddr::new(IpAddr::V4(ip), port),
        Address::V6(ip, port) => SocketAddr::new(IpAddr::V6(ip), port),
        Address::Domain(_, _) => panic!("reply must advertise a concrete address"),
    }
}

async fn udp_echo(a: Answer, aaaa: Answer, ipv6: bool, advertise: bool) {
    let dns = DnsFixture::start(a, aaaa).await;
    let mut config = dns.config(ipv6);
    if advertise {
        config.udp.advertise = Some(next_socks5::config::AdvertiseHost::Domain(HOST.to_owned()));
    }
    let proxy = Proxy::start(config).await;
    let bind = if ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
    let upstream = UdpSocket::bind(bind).await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let (_control, code, relay) = proxy
        .request(3, Address::V4(Ipv4Addr::UNSPECIFIED, 0))
        .await;
    assert_eq!(code, 0, "UDP advertise resolution must succeed");
    let relay = socket_address(relay);
    assert_eq!(
        relay.is_ipv6(),
        ipv6,
        "advertised address must match the relay family"
    );
    let client = UdpSocket::bind(bind).await.unwrap();
    let mut request = Vec::new();
    encap(
        &Address::Domain(HOST.to_owned(), upstream_address.port()),
        b"udp ping",
        &mut request,
    );
    timeout(IO_TIMEOUT, async {
        client.send_to(&request, relay).await.unwrap();
        let mut buffer = [0u8; 1024];
        let (length, sender) = upstream.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..length], b"udp ping");
        upstream.send_to(&buffer[..length], sender).await.unwrap();
        let (length, _) = client.recv_from(&mut buffer).await.unwrap();
        let reply = decap(&buffer[..length]).unwrap();
        assert_eq!(reply.data, b"udp ping");
        assert_eq!(socket_address(reply.address), upstream_address);
    })
    .await
    .expect("UDP DNS relay did not complete");
}

#[tokio::test]
async fn udp_a_survives_aaaa_servfail() {
    udp_echo(v4(), Answer::ServFail, false, false).await;
}

#[tokio::test]
async fn udp_a_survives_aaaa_nxdomain() {
    udp_echo(v4(), Answer::NxDomain, false, false).await;
}

#[tokio::test]
async fn udp_advertise_and_target_use_ipv4_from_dual_stack_dns() {
    udp_echo(v4(), v6(), false, true).await;
}

#[tokio::test]
async fn udp_advertise_and_target_use_ipv6_from_dual_stack_dns() {
    udp_echo(v4(), v6(), true, true).await;
}

#[tokio::test]
async fn udp_loopback_dns_answer_is_blocked() {
    let dns = DnsFixture::start(v4(), empty()).await;
    let mut config = dns.config(false);
    config.egress = Egress::default();
    let proxy = Proxy::start(config).await;
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (_control, code, relay) = proxy
        .request(3, Address::V4(Ipv4Addr::UNSPECIFIED, 0))
        .await;
    assert_eq!(code, 0);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut request = Vec::new();
    encap(
        &Address::Domain(HOST.to_owned(), upstream.local_addr().unwrap().port()),
        b"blocked",
        &mut request,
    );
    client
        .send_to(&request, socket_address(relay))
        .await
        .unwrap();
    dns.wait_for_queries(1).await;
    let mut buffer = [0u8; 64];
    assert!(
        timeout(Duration::from_millis(200), upstream.recv_from(&mut buffer))
            .await
            .is_err(),
        "UDP must not forward a domain resolving to a blocked loopback address"
    );
}

#[tokio::test]
async fn tcp_aaaa_survives_a_servfail() {
    tcp_echo(Answer::ServFail, v6(), true).await;
}

#[tokio::test]
async fn tcp_aaaa_survives_a_nxdomain() {
    tcp_echo(Answer::NxDomain, v6(), true).await;
}

#[tokio::test]
async fn tcp_empty_dns_answers_report_no_addresses() {
    let dns = DnsFixture::start(empty(), empty()).await;
    let mut proxy = Proxy::start(dns.config(false)).await;
    let (_, code, _) = proxy
        .request(1, Address::Domain(HOST.to_owned(), 443))
        .await;
    assert_eq!(code, 4);
    let message = proxy.error().await;
    assert!(message.contains(HOST), "missing DNS target: {message}");
    assert!(
        message.contains("no addresses"),
        "missing empty-result diagnosis: {message}"
    );
}

#[tokio::test]
async fn udp_ipv6_only_dns_succeeds() {
    udp_echo(empty(), v6(), true, false).await;
}

#[test]
fn dns_config_defaults_to_system_resolvers() {
    let config = Config::from_toml_str("listen = \"127.0.0.1:0\"").unwrap();
    assert!(config.dns.servers.is_empty());
}

#[test]
fn dns_config_accepts_literal_servers_and_optional_ports() {
    let config = Config::from_toml_str(
        r#"listen = "127.0.0.1:0"
[dns]
servers = ["192.0.2.1", "2001:db8::1", "127.0.0.1:5353", "[::1]:5354"]
"#,
    )
    .unwrap();
    assert_eq!(
        config.dns.servers,
        [
            "192.0.2.1:53",
            "[2001:db8::1]:53",
            "127.0.0.1:5353",
            "[::1]:5354"
        ]
        .map(|address| address.parse::<SocketAddr>().unwrap())
    );
}

#[test]
fn dns_config_rejects_invalid_upstreams() {
    for server in [
        "resolver.example",
        "127.0.0.1:0",
        "0.0.0.0",
        "::",
        "224.0.0.1",
        "ff02::1",
        "",
        "127.0.0.1:65536",
    ] {
        let config = format!("listen = \"127.0.0.1:0\"\n[dns]\nservers = [{server:?}]\n");
        assert!(
            Config::from_toml_str(&config).is_err(),
            "must reject {server:?}"
        );
    }
    assert!(
        Config::from_toml_str("listen = \"127.0.0.1:0\"\n[dns]\nserver = [\"127.0.0.1\"]").is_err()
    );
}

#[tokio::test]
async fn udp_dns_cache_expires_at_record_ttl() {
    let dns = DnsFixture::with_ttl(v4(), empty(), 1).await;
    let proxy = Proxy::start(dns.config(false)).await;
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (_control, code, relay) = proxy
        .request(3, Address::V4(Ipv4Addr::UNSPECIFIED, 0))
        .await;
    assert_eq!(code, 0);
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut request = Vec::new();
    encap(
        &Address::Domain(HOST.to_owned(), upstream.local_addr().unwrap().port()),
        b"cache",
        &mut request,
    );
    let relay = socket_address(relay);
    let mut buffer = [0u8; 64];
    for index in 0..3 {
        if index == 2 {
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        client.send_to(&request, relay).await.unwrap();
        let (length, _) = timeout(IO_TIMEOUT, upstream.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..length], b"cache");
        let expected = if index == 2 { 2 } else { 1 };
        assert_eq!(
            dns.query_count.load(Ordering::SeqCst),
            expected,
            "DNS cache must reuse an unexpired answer and refresh after its TTL"
        );
    }
}

#[tokio::test]
async fn tcp_a_survives_silent_aaaa_server() {
    tcp_echo(v4(), Answer::Silent, false).await;
}

#[tokio::test]
async fn tcp_aaaa_survives_silent_a_server() {
    tcp_echo(Answer::Silent, v6(), true).await;
}

#[tokio::test]
async fn tcp_waits_for_allowed_family_when_fast_answer_is_blocked() {
    tcp_echo_with_egress(
        Answer::Addresses(vec!["10.0.0.1".parse().unwrap()]),
        Answer::Delayed(Duration::from_millis(350), Box::new(v6())),
        true,
        Egress {
            block_loopback: false,
            ..Egress::default()
        },
    )
    .await;
}

#[tokio::test]
async fn tcp_skips_blocked_dns_candidates_before_connecting() {
    tcp_echo_with_egress(
        Answer::Addresses(vec![
            "10.0.0.1".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ]),
        empty(),
        false,
        Egress {
            block_loopback: false,
            ..Egress::default()
        },
    )
    .await;
}

#[tokio::test]
async fn tcp_waits_for_slow_family_after_fast_connection_is_refused() {
    tcp_echo(
        v4(),
        Answer::Delayed(Duration::from_millis(350), Box::new(v6())),
        true,
    )
    .await;
}

#[test]
fn dns_config_rejects_unsupported_ipv6_scope() {
    let config =
        Config::from_toml_str("listen = \"127.0.0.1:0\"\n[dns]\nservers = [\"[fe80::1%2]:53\"]\n");
    assert!(
        config.is_err(),
        "IPv6 upstream scopes must not be silently discarded"
    );
}
