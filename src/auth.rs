use std::sync::Arc;

use subtle::ConstantTimeEq;

use crate::credentials::SecretToken;

/// Authenticates an HTTP bearer credential.
pub trait Authenticator: Send + Sync {
    /// Returns true only when the credential is authorized.
    fn authenticate(&self, bearer_token: &str) -> bool;
}

/// Constant-time authenticator backed by one static token.
#[derive(Clone)]
pub struct StaticTokenAuthenticator {
    expected: Arc<SecretToken>,
}

impl StaticTokenAuthenticator {
    /// Creates an authenticator. The token value is never formatted or logged.
    pub fn new(token: SecretToken) -> Self {
        Self {
            expected: Arc::new(token),
        }
    }
}

impl Authenticator for StaticTokenAuthenticator {
    fn authenticate(&self, bearer_token: &str) -> bool {
        let supplied = bearer_token.as_bytes();
        let expected = self.expected.expose().as_bytes();
        let same_length = (supplied.len() as u64).ct_eq(&(expected.len() as u64));
        let mut difference = 0_u8;
        let maximum = supplied.len().max(expected.len());
        for index in 0..maximum {
            let left = supplied.get(index).copied().unwrap_or_default();
            let right = expected.get(index).copied().unwrap_or_default();
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

    fn token(value: &str) -> SecretToken {
        SecretToken::new(value.to_owned()).unwrap()
    }

    #[test]
    fn accepts_only_the_correct_token() {
        let auth = StaticTokenAuthenticator::new(token("correct"));
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
