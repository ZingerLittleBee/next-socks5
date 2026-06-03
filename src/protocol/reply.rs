//! SOCKS5 reply codec (RFC 1928 section 6): pure encoding, no IO.

use crate::protocol::address::Address;

/// REP field value indicating the request succeeded.
pub const REP_SUCCEEDED: u8 = 0x00;

const VERSION: u8 = 0x05;
const RSV: u8 = 0x00;

/// Append a SOCKS5 reply to `out`: `VER(0x05) REP RSV(0x00) ATYP BND.ADDR BND.PORT`.
pub fn encode_reply(code: u8, bind: &Address, out: &mut Vec<u8>) {
    out.push(VERSION);
    out.push(code);
    out.push(RSV);
    bind.encode(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn encode_success_ipv4() {
        let mut out = Vec::new();
        encode_reply(
            0x00,
            &Address::V4(Ipv4Addr::new(127, 0, 0, 1), 1080),
            &mut out,
        );
        assert_eq!(
            out,
            vec![0x05, 0x00, 0x00, 0x01, 0x7f, 0x00, 0x00, 0x01, 0x04, 0x38]
        );
    }
}
