//! SOCKS5 error types and their mapping to RFC 1928 reply codes.

use std::io;

/// A SOCKS5 failure condition, each corresponding to an RFC 1928 reply code.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum Socks5Error {
    #[error("general SOCKS server failure")]
    General,
    #[error("connection not allowed by ruleset")]
    NotAllowed,
    #[error("network unreachable")]
    NetworkUnreachable,
    #[error("host unreachable")]
    HostUnreachable,
    #[error("connection refused")]
    ConnectionRefused,
    #[error("TTL expired")]
    TtlExpired,
    #[error("command not supported")]
    CommandNotSupported,
    #[error("address type not supported")]
    AddressNotSupported,
}

impl Socks5Error {
    /// Return the RFC 1928 reply code (REP field) for this error.
    pub fn reply_code(&self) -> u8 {
        match self {
            Socks5Error::General => 0x01,
            Socks5Error::NotAllowed => 0x02,
            Socks5Error::NetworkUnreachable => 0x03,
            Socks5Error::HostUnreachable => 0x04,
            Socks5Error::ConnectionRefused => 0x05,
            Socks5Error::TtlExpired => 0x06,
            Socks5Error::CommandNotSupported => 0x07,
            Socks5Error::AddressNotSupported => 0x08,
        }
    }

    /// Map an [`io::Error`] to the closest SOCKS5 error.
    ///
    /// Only stable [`io::ErrorKind`] variants are matched; anything else maps
    /// to [`Socks5Error::General`].
    pub fn from_io(e: &io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NetworkUnreachable => Socks5Error::NetworkUnreachable,
            io::ErrorKind::HostUnreachable => Socks5Error::HostUnreachable,
            io::ErrorKind::ConnectionRefused => Socks5Error::ConnectionRefused,
            io::ErrorKind::TimedOut => Socks5Error::TtlExpired,
            io::ErrorKind::AddrNotAvailable => Socks5Error::HostUnreachable,
            _ => Socks5Error::General,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_codes() {
        assert_eq!(Socks5Error::General.reply_code(), 0x01);
        assert_eq!(Socks5Error::NotAllowed.reply_code(), 0x02);
        assert_eq!(Socks5Error::NetworkUnreachable.reply_code(), 0x03);
        assert_eq!(Socks5Error::HostUnreachable.reply_code(), 0x04);
        assert_eq!(Socks5Error::ConnectionRefused.reply_code(), 0x05);
        assert_eq!(Socks5Error::TtlExpired.reply_code(), 0x06);
        assert_eq!(Socks5Error::CommandNotSupported.reply_code(), 0x07);
        assert_eq!(Socks5Error::AddressNotSupported.reply_code(), 0x08);
    }

    #[test]
    fn from_io_connection_refused() {
        let e = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(Socks5Error::from_io(&e).reply_code(), 0x05);
    }

    #[test]
    fn from_io_timed_out() {
        let e = io::Error::from(io::ErrorKind::TimedOut);
        assert_eq!(Socks5Error::from_io(&e).reply_code(), 0x06);
    }

    #[test]
    fn from_io_network_unreachable() {
        let e = io::Error::from(io::ErrorKind::NetworkUnreachable);
        assert_eq!(Socks5Error::from_io(&e).reply_code(), 0x03);
    }

    #[test]
    fn from_io_host_unreachable() {
        let e = io::Error::from(io::ErrorKind::HostUnreachable);
        assert_eq!(Socks5Error::from_io(&e).reply_code(), 0x04);
    }
}
