//! SOCKS5 request codec (RFC 1928 section 4): pure parsing, no IO.

use crate::protocol::address::{AddrError, Address};

/// SOCKS5 request command (the CMD field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Connect,
    Bind,
    UdpAssociate,
}

/// A parsed SOCKS5 request: a command paired with its target address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub command: Command,
    pub address: Address,
}

/// Errors produced while parsing a SOCKS5 request.
#[derive(Debug, PartialEq, Eq)]
pub enum RequestError {
    /// Buffer ended before the fixed header could be read.
    Truncated,
    /// VER field was not 0x05.
    BadVersion(u8),
    /// CMD field was not a known command.
    BadCommand(u8),
    /// The address portion failed to decode.
    Addr(AddrError),
}

const VERSION: u8 = 0x05;
const CMD_CONNECT: u8 = 0x01;
const CMD_BIND: u8 = 0x02;
const CMD_UDP_ASSOCIATE: u8 = 0x03;

/// Parse a SOCKS5 request: `VER CMD RSV ATYP DST.ADDR DST.PORT`.
///
/// The RSV byte is ignored. The address (from byte 3 onward) is decoded via
/// [`Address::decode`].
pub fn parse_request(buf: &[u8]) -> Result<Request, RequestError> {
    // Fixed header is VER CMD RSV ATYP; need at least 4 bytes.
    if buf.len() < 4 {
        return Err(RequestError::Truncated);
    }
    if buf[0] != VERSION {
        return Err(RequestError::BadVersion(buf[0]));
    }
    let command = match buf[1] {
        CMD_CONNECT => Command::Connect,
        CMD_BIND => Command::Bind,
        CMD_UDP_ASSOCIATE => Command::UdpAssociate,
        other => return Err(RequestError::BadCommand(other)),
    };
    // buf[2] is RSV, ignored. Address begins at byte 3 (ATYP).
    let (address, _consumed) = Address::decode(&buf[3..]).map_err(RequestError::Addr)?;
    Ok(Request { command, address })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn parse_connect_ipv4() {
        // VER CMD RSV ATYP=0x01 1.2.3.4 port 80
        let buf = [0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50];
        let req = parse_request(&buf).unwrap();
        assert_eq!(req.command, Command::Connect);
        assert_eq!(req.address, Address::V4(Ipv4Addr::new(1, 2, 3, 4), 80));
    }

    #[test]
    fn parse_udp_associate_domain() {
        // VER CMD=0x03 RSV ATYP=0x03 len=11 "example.com" port 53
        let mut buf = vec![0x05, 0x03, 0x00, 0x03, 11];
        buf.extend_from_slice(b"example.com");
        buf.extend_from_slice(&53u16.to_be_bytes());
        let req = parse_request(&buf).unwrap();
        assert_eq!(req.command, Command::UdpAssociate);
        assert_eq!(req.address, Address::Domain("example.com".to_owned(), 53));
    }

    #[test]
    fn parse_bind() {
        let buf = [0x05, 0x02, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50];
        let req = parse_request(&buf).unwrap();
        assert_eq!(req.command, Command::Bind);
    }

    #[test]
    fn bad_version() {
        let buf = [0x04, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50];
        assert_eq!(parse_request(&buf), Err(RequestError::BadVersion(4)));
    }

    #[test]
    fn bad_command() {
        let buf = [0x05, 0x09, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50];
        assert_eq!(parse_request(&buf), Err(RequestError::BadCommand(9)));
    }

    #[test]
    fn truncated() {
        let buf = [0x05, 0x01];
        assert_eq!(parse_request(&buf), Err(RequestError::Truncated));
    }
}
