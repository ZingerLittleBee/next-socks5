//! SOCKS5 address codec (RFC 1928 section 4): pure encode/decode, no IO.

use std::net::{Ipv4Addr, Ipv6Addr};

/// A SOCKS5 address: an IPv4/IPv6 endpoint or a domain name, each with a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    V4(Ipv4Addr, u16),
    V6(Ipv6Addr, u16),
    Domain(String, u16),
}

/// Errors produced while decoding a SOCKS5 address.
#[derive(Debug, PartialEq, Eq)]
pub enum AddrError {
    /// Buffer ended before the full address could be read.
    Truncated,
    /// Unknown ATYP byte.
    BadAtyp(u8),
    /// Domain name was not valid UTF-8.
    BadDomain,
}

const ATYP_V4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_V6: u8 = 0x04;

impl Address {
    /// Decode an address from the start of `buf`.
    ///
    /// Returns the parsed [`Address`] and the number of bytes consumed. Any
    /// trailing bytes in `buf` are left untouched.
    pub fn decode(buf: &[u8]) -> Result<(Address, usize), AddrError> {
        let atyp = *buf.first().ok_or(AddrError::Truncated)?;
        match atyp {
            ATYP_V4 => {
                // 1 (ATYP) + 4 (addr) + 2 (port)
                if buf.len() < 7 {
                    return Err(AddrError::Truncated);
                }
                let octets: [u8; 4] = buf[1..5].try_into().unwrap();
                let port = u16::from_be_bytes([buf[5], buf[6]]);
                Ok((Address::V4(Ipv4Addr::from(octets), port), 7))
            }
            ATYP_V6 => {
                // 1 (ATYP) + 16 (addr) + 2 (port)
                if buf.len() < 19 {
                    return Err(AddrError::Truncated);
                }
                let octets: [u8; 16] = buf[1..17].try_into().unwrap();
                let port = u16::from_be_bytes([buf[17], buf[18]]);
                Ok((Address::V6(Ipv6Addr::from(octets), port), 19))
            }
            ATYP_DOMAIN => {
                // 1 (ATYP) + 1 (len) + N (domain) + 2 (port)
                let len = *buf.get(1).ok_or(AddrError::Truncated)? as usize;
                let total = 2 + len + 2;
                if buf.len() < total {
                    return Err(AddrError::Truncated);
                }
                let domain = std::str::from_utf8(&buf[2..2 + len])
                    .map_err(|_| AddrError::BadDomain)?
                    .to_owned();
                let port = u16::from_be_bytes([buf[2 + len], buf[2 + len + 1]]);
                Ok((Address::Domain(domain, port), total))
            }
            other => Err(AddrError::BadAtyp(other)),
        }
    }

    /// Encode this address (ATYP + address + port) onto the end of `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Address::V4(ip, port) => {
                out.push(ATYP_V4);
                out.extend_from_slice(&ip.octets());
                out.extend_from_slice(&port.to_be_bytes());
            }
            Address::V6(ip, port) => {
                out.push(ATYP_V6);
                out.extend_from_slice(&ip.octets());
                out.extend_from_slice(&port.to_be_bytes());
            }
            Address::Domain(domain, port) => {
                out.push(ATYP_DOMAIN);
                // Domain length is bounded to a single byte by the wire format.
                out.push(domain.len() as u8);
                out.extend_from_slice(domain.as_bytes());
                out.extend_from_slice(&port.to_be_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_round_trip() {
        let addr = Address::V4(Ipv4Addr::new(127, 0, 0, 1), 1080);
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let (decoded, consumed) = Address::decode(&buf).unwrap();
        assert_eq!(decoded, addr);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn v6_round_trip() {
        let addr = Address::V6(Ipv6Addr::LOCALHOST, 443);
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let (decoded, consumed) = Address::decode(&buf).unwrap();
        assert_eq!(decoded, addr);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn domain_round_trip() {
        let addr = Address::Domain("example.com".to_owned(), 80);
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let (decoded, consumed) = Address::decode(&buf).unwrap();
        assert_eq!(decoded, addr);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn truncated_v4() {
        // ATYP=0x01 but only 2 trailing bytes.
        let buf = [ATYP_V4, 0x7f, 0x00];
        assert_eq!(Address::decode(&buf), Err(AddrError::Truncated));
    }

    #[test]
    fn bad_atyp() {
        let buf = [0x09, 0x00, 0x00];
        assert_eq!(Address::decode(&buf), Err(AddrError::BadAtyp(9)));
    }

    #[test]
    fn max_domain_round_trip() {
        let domain = "a".repeat(255);
        let addr = Address::Domain(domain, 12345);
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let (decoded, consumed) = Address::decode(&buf).unwrap();
        assert_eq!(decoded, addr);
        // 1 (ATYP) + 1 (len) + 255 (domain) + 2 (port).
        assert_eq!(consumed, 259);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn trailing_bytes_not_consumed() {
        let addr = Address::V4(Ipv4Addr::new(10, 0, 0, 1), 8080);
        let mut buf = Vec::new();
        addr.encode(&mut buf);
        let encoded_len = buf.len();
        buf.push(0xAA);
        let (decoded, consumed) = Address::decode(&buf).unwrap();
        assert_eq!(decoded, addr);
        assert_eq!(consumed, encoded_len);
        assert_eq!(buf[consumed], 0xAA);
    }

    #[test]
    fn domain_invalid_utf8() {
        // ATYP=0x03, len=2, two invalid UTF-8 bytes, then port.
        let buf = [ATYP_DOMAIN, 0x02, 0xff, 0xfe, 0x00, 0x50];
        assert_eq!(Address::decode(&buf), Err(AddrError::BadDomain));
    }
}
