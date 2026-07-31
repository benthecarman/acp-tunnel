use std::sync::Arc;

use subtle::ConstantTimeEq;

/// Authenticates an HTTP bearer credential.
pub trait Authenticator: Send + Sync {
    /// Returns true only when the credential is authorized.
    fn authenticate(&self, bearer_token: &str) -> bool;
}

/// Constant-time authenticator backed by one static token.
#[derive(Clone)]
pub struct StaticTokenAuthenticator {
    expected: Arc<[u8]>,
}

impl StaticTokenAuthenticator {
    /// Creates an authenticator. The token value is never formatted or logged.
    pub fn new(token: impl AsRef<[u8]>) -> Self {
        Self {
            expected: Arc::from(token.as_ref()),
        }
    }
}

impl Authenticator for StaticTokenAuthenticator {
    fn authenticate(&self, bearer_token: &str) -> bool {
        let supplied = bearer_token.as_bytes();
        let same_length = (supplied.len() as u64).ct_eq(&(self.expected.len() as u64));
        let mut difference = 0_u8;
        let maximum = supplied.len().max(self.expected.len());
        for index in 0..maximum {
            let left = supplied.get(index).copied().unwrap_or_default();
            let right = self.expected.get(index).copied().unwrap_or_default();
            difference |= left ^ right;
        }
        bool::from(same_length & difference.ct_eq(&0))
    }
}

/// Extracts the token from a strict `Bearer <token>` header.
pub fn parse_bearer(header: &str) -> Option<&str> {
    let (scheme, credential) = header.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer")
        && !credential.is_empty()
        && !credential.contains(char::is_whitespace)
    {
        Some(credential)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_correct_token() {
        let auth = StaticTokenAuthenticator::new("correct");
        assert!(auth.authenticate("correct"));
        assert!(!auth.authenticate("wrong"));
        assert!(!auth.authenticate("correct-but-longer"));
        assert!(!auth.authenticate(""));
    }

    #[test]
    fn bearer_parsing_is_strict() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer a b"), None);
        assert_eq!(parse_bearer("Bearer "), None);
    }
}
