//! SOCKS5 UDP datagram header codec (RFC 1928 section 7): pure encode/decode, no IO.

use crate::protocol::address::{AddrError, Address};

/// A decapsulated SOCKS5 UDP request datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpDatagram {
    /// Fragment number, returned as-is (caller decides to drop FRAG != 0).
    pub frag: u8,
    /// Destination address (DST.ADDR + DST.PORT).
    pub address: Address,
    /// Application payload following the address.
    pub data: Vec<u8>,
}

/// Errors produced while decapsulating a SOCKS5 UDP datagram.
#[derive(Debug, PartialEq, Eq)]
pub enum UdpError {
    /// Buffer ended before the full header could be read.
    Truncated,
    /// Address could not be decoded.
    Addr(AddrError),
}

/// A decapsulated SOCKS5 UDP datagram whose payload borrows the input buffer
/// (the relay hot path uses this to avoid a per-datagram copy).
#[derive(Debug, PartialEq, Eq)]
pub struct UdpDatagramRef<'a> {
    /// Fragment number, returned as-is (caller decides to drop FRAG != 0).
    pub frag: u8,
    /// Destination address (DST.ADDR + DST.PORT).
    pub address: Address,
    /// Application payload following the address, borrowed from the input.
    pub data: &'a [u8],
}

// RSV(2) + FRAG(1) + ATYP(1): the minimum bytes before address parsing.
const MIN_HEADER: usize = 4;

/// Parse a SOCKS5 UDP datagram, borrowing the payload from `buf`.
///
/// FRAG is returned as-is so the caller can decide to drop fragmented
/// datagrams (FRAG != 0).
pub fn decap_ref(buf: &[u8]) -> Result<UdpDatagramRef<'_>, UdpError> {
    if buf.len() < MIN_HEADER {
        return Err(UdpError::Truncated);
    }
    // buf[0], buf[1] are RSV (0x0000) and ignored; buf[2] is FRAG.
    let frag = buf[2];
    // From byte 3 onward (ATYP + address + port) is parsed by Address::decode.
    let (address, consumed) = Address::decode(&buf[3..]).map_err(UdpError::Addr)?;
    Ok(UdpDatagramRef {
        frag,
        address,
        data: &buf[3 + consumed..],
    })
}

/// Parse a SOCKS5 UDP datagram into an owned [`UdpDatagram`].
pub fn decap(buf: &[u8]) -> Result<UdpDatagram, UdpError> {
    let d = decap_ref(buf)?;
    Ok(UdpDatagram {
        frag: d.frag,
        address: d.address,
        data: d.data.to_vec(),
    })
}

/// Encode RSV(0x0000) FRAG(0x00) ATYP DST.ADDR DST.PORT DATA into `out`.
pub fn encap(address: &Address, data: &[u8], out: &mut Vec<u8>) {
    out.push(0x00); // RSV
    out.push(0x00); // RSV
    out.push(0x00); // FRAG
    address.encode(out);
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn round_trip_domain() {
        let addr = Address::Domain("example.com".to_owned(), 53);
        let mut buf = Vec::new();
        encap(&addr, b"hello", &mut buf);
        let dg = decap(&buf).unwrap();
        assert_eq!(dg.frag, 0);
        assert_eq!(dg.address, addr);
        assert_eq!(dg.data, b"hello");
    }

    #[test]
    fn round_trip_v4() {
        let addr = Address::V4(Ipv4Addr::new(8, 8, 8, 8), 53);
        let mut buf = Vec::new();
        encap(&addr, b"\x01\x02\x03", &mut buf);
        let dg = decap(&buf).unwrap();
        assert_eq!(dg.frag, 0);
        assert_eq!(dg.address, addr);
        assert_eq!(dg.data, b"\x01\x02\x03");
    }

    #[test]
    fn decap_preserves_frag() {
        // Manually build a datagram with FRAG=3 and an IPv4 target.
        let addr = Address::V4(Ipv4Addr::new(1, 1, 1, 1), 80);
        let mut buf = vec![0x00, 0x00, 0x03];
        addr.encode(&mut buf);
        buf.extend_from_slice(b"data");
        let dg = decap(&buf).unwrap();
        assert_eq!(dg.frag, 3);
        assert_eq!(dg.address, addr);
        assert_eq!(dg.data, b"data");
    }

    #[test]
    fn decap_truncated_header() {
        assert_eq!(decap(&[0x00, 0x00]), Err(UdpError::Truncated));
    }

    #[test]
    fn encap_starts_with_rsv_and_frag() {
        let addr = Address::V4(Ipv4Addr::new(127, 0, 0, 1), 1080);
        let mut buf = Vec::new();
        encap(&addr, b"", &mut buf);
        assert_eq!(&buf[..3], &[0x00, 0x00, 0x00]);
    }

    #[test]
    fn decap_ref_borrows_payload() {
        let addr = Address::Domain("example.com".to_owned(), 53);
        let mut buf = Vec::new();
        encap(&addr, b"hello", &mut buf);
        let dg = decap_ref(&buf).unwrap();
        assert_eq!(dg.frag, 0);
        assert_eq!(dg.address, addr);
        assert_eq!(dg.data, b"hello");
        // The payload is a view into the input buffer, not a copy.
        assert_eq!(dg.data.as_ptr(), buf[buf.len() - 5..].as_ptr());
    }

    #[test]
    fn decap_empty_data() {
        let addr = Address::V4(Ipv4Addr::new(10, 0, 0, 1), 8080);
        let mut buf = Vec::new();
        encap(&addr, b"", &mut buf);
        let dg = decap(&buf).unwrap();
        assert_eq!(dg.address, addr);
        assert!(dg.data.is_empty());
    }
}
