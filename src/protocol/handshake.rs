//! SOCKS5 handshake (RFC 1928 section 3) and username/password auth
//! (RFC 1929): pure encode/decode, no IO.

/// SOCKS protocol version.
pub const VERSION: u8 = 0x05;
/// Method: no authentication required.
pub const METHOD_NO_AUTH: u8 = 0x00;
/// Method: username/password authentication (RFC 1929).
pub const METHOD_USERPASS: u8 = 0x02;
/// Method: no acceptable methods offered by the client.
pub const METHOD_NONE_ACCEPTABLE: u8 = 0xFF;

/// RFC 1929 username/password auth subnegotiation version.
const USERPASS_VERSION: u8 = 0x01;

/// Errors produced while decoding handshake/auth messages.
#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// Buffer ended before the full message could be read.
    Truncated,
    /// Unexpected version byte.
    BadVersion(u8),
    /// A length-prefixed string was not valid UTF-8.
    BadDomain,
}

/// Parse a client greeting (VER NMETHODS METHODS...), returning the offered
/// methods list.
pub fn parse_greeting(buf: &[u8]) -> Result<Vec<u8>, HandshakeError> {
    let ver = *buf.first().ok_or(HandshakeError::Truncated)?;
    if ver != VERSION {
        return Err(HandshakeError::BadVersion(ver));
    }
    let nmethods = *buf.get(1).ok_or(HandshakeError::Truncated)? as usize;
    let methods = buf.get(2..2 + nmethods).ok_or(HandshakeError::Truncated)?;
    Ok(methods.to_vec())
}

/// Choose an authentication method from the client's offered list.
///
/// When `require_userpass` is set, only [`METHOD_USERPASS`] is acceptable;
/// otherwise only [`METHOD_NO_AUTH`] is acceptable. Returns
/// [`METHOD_NONE_ACCEPTABLE`] when the required method was not offered.
pub fn select_method(offered: &[u8], require_userpass: bool) -> u8 {
    let wanted = if require_userpass {
        METHOD_USERPASS
    } else {
        METHOD_NO_AUTH
    };
    if offered.contains(&wanted) {
        wanted
    } else {
        METHOD_NONE_ACCEPTABLE
    }
}

/// Build a server method-selection reply: `[VERSION, method]`.
pub fn method_reply(method: u8) -> [u8; 2] {
    [VERSION, method]
}

/// Parse an RFC 1929 username/password request
/// (VER ULEN UNAME PLEN PASSWD), returning `(username, password)`.
pub fn parse_userpass(buf: &[u8]) -> Result<(String, String), HandshakeError> {
    let ver = *buf.first().ok_or(HandshakeError::Truncated)?;
    if ver != USERPASS_VERSION {
        return Err(HandshakeError::BadVersion(ver));
    }
    let ulen = *buf.get(1).ok_or(HandshakeError::Truncated)? as usize;
    let uname = buf.get(2..2 + ulen).ok_or(HandshakeError::Truncated)?;
    let plen_idx = 2 + ulen;
    let plen = *buf.get(plen_idx).ok_or(HandshakeError::Truncated)? as usize;
    let passwd = buf
        .get(plen_idx + 1..plen_idx + 1 + plen)
        .ok_or(HandshakeError::Truncated)?;
    let username = std::str::from_utf8(uname)
        .map_err(|_| HandshakeError::BadDomain)?
        .to_owned();
    let password = std::str::from_utf8(passwd)
        .map_err(|_| HandshakeError::BadDomain)?
        .to_owned();
    Ok((username, password))
}

/// Build an RFC 1929 auth reply: `[0x01, 0x00]` on success, `[0x01, 0x01]`
/// on failure.
pub fn userpass_reply(ok: bool) -> [u8; 2] {
    [USERPASS_VERSION, if ok { 0x00 } else { 0x01 }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_parses_methods() {
        assert_eq!(
            parse_greeting(&[0x05, 0x02, 0x00, 0x02]).unwrap(),
            vec![0x00, 0x02]
        );
    }

    #[test]
    fn greeting_truncated() {
        assert_eq!(
            parse_greeting(&[0x05, 0x02, 0x00]),
            Err(HandshakeError::Truncated)
        );
    }

    #[test]
    fn greeting_bad_version() {
        assert_eq!(
            parse_greeting(&[0x04, 0x01, 0x00]),
            Err(HandshakeError::BadVersion(4))
        );
    }

    #[test]
    fn select_userpass_when_required_and_offered() {
        assert_eq!(select_method(&[0x00, 0x02], true), METHOD_USERPASS);
    }

    #[test]
    fn select_none_when_userpass_required_but_missing() {
        assert_eq!(select_method(&[0x00], true), METHOD_NONE_ACCEPTABLE);
    }

    #[test]
    fn select_no_auth_when_not_required() {
        assert_eq!(select_method(&[0x00, 0x02], false), METHOD_NO_AUTH);
    }

    #[test]
    fn select_none_when_no_auth_missing() {
        assert_eq!(select_method(&[0x02], false), METHOD_NONE_ACCEPTABLE);
    }

    #[test]
    fn userpass_round_trip() {
        let user = "alice";
        let pass = "secret";
        let mut buf = vec![USERPASS_VERSION, user.len() as u8];
        buf.extend_from_slice(user.as_bytes());
        buf.push(pass.len() as u8);
        buf.extend_from_slice(pass.as_bytes());
        assert_eq!(
            parse_userpass(&buf).unwrap(),
            ("alice".to_owned(), "secret".to_owned())
        );
    }

    #[test]
    fn userpass_truncated() {
        // VER, ULEN=5 but username bytes are missing.
        assert_eq!(
            parse_userpass(&[0x01, 0x05, b'a']),
            Err(HandshakeError::Truncated)
        );
    }

    #[test]
    fn userpass_bad_version() {
        assert_eq!(
            parse_userpass(&[0x02, 0x01, b'a', 0x01, b'b']),
            Err(HandshakeError::BadVersion(2))
        );
    }

    #[test]
    fn replies_match_spec() {
        assert_eq!(method_reply(METHOD_USERPASS), [0x05, 0x02]);
        assert_eq!(userpass_reply(true), [0x01, 0x00]);
        assert_eq!(userpass_reply(false), [0x01, 0x01]);
    }
}
