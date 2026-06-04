//! Constant-time credential verification (RFC 1929 username/password).
//!
//! A plain `==` comparison short-circuits on the first differing byte, leaking
//! how many leading bytes matched via timing. These helpers compare in constant
//! time per pair and check every configured user without early exit.

use crate::config::User;

/// Constant-time byte-slice equality.
///
/// Returns early only on a length mismatch (lengths are not secret); otherwise
/// every byte is folded into an accumulator so the running time does not depend
/// on the position of the first difference.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify a username/password pair against the configured users.
///
/// Every user is checked (no short-circuit on the first match) so the timing
/// does not reveal which entry matched or how far the scan got.
pub fn verify_credentials(users: &[User], username: &str, password: &str) -> bool {
    let mut ok = false;
    for u in users {
        let user_match = ct_eq(u.username.as_bytes(), username.as_bytes());
        let pass_match = ct_eq(u.password.as_bytes(), password.as_bytes());
        ok |= user_match & pass_match;
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<User> {
        vec![
            User {
                username: "alice".into(),
                password: "secret".into(),
            },
            User {
                username: "bob".into(),
                password: "hunter2".into(),
            },
        ]
    }

    #[test]
    fn ct_eq_matches_and_differs() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn accepts_correct_credentials() {
        assert!(verify_credentials(&users(), "alice", "secret"));
        assert!(verify_credentials(&users(), "bob", "hunter2"));
    }

    #[test]
    fn rejects_wrong_password() {
        assert!(!verify_credentials(&users(), "alice", "wrong"));
    }

    #[test]
    fn rejects_unknown_user() {
        assert!(!verify_credentials(&users(), "carol", "secret"));
    }

    #[test]
    fn rejects_when_no_users_configured() {
        assert!(!verify_credentials(&[], "alice", "secret"));
    }
}
