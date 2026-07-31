use std::{
    ffi::OsString,
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{Error, Result};

const MAX_TOKEN_FILE_BYTES: u64 = 16 * 1024;
const DEFAULT_TOKEN_FILE: &str = ".config/acp-tunnel/token";

/// A bearer token whose debug representation never reveals its value.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretToken(Arc<str>);

impl SecretToken {
    /// Creates a nonempty secret token for an embedding application.
    pub fn new(value: String) -> Result<Self> {
        if value.is_empty() {
            return Err(Error::Config("tunnel credential must not be empty".into()));
        }
        Ok(Self(Arc::from(value)))
    }

    /// Returns the token for authentication or authorization-header construction.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

/// Loads one tunnel credential from an explicit source or the default file.
pub fn load_token(cli_token_file: Option<&Path>) -> Result<SecretToken> {
    load_token_from_sources(
        cli_token_file.map(Path::to_path_buf),
        std::env::var_os("ACP_TUNNEL_TOKEN_FILE"),
        std::env::var_os("ACP_TUNNEL_TOKEN"),
        default_token_file(),
    )
}

fn load_token_from_sources(
    cli_token_file: Option<PathBuf>,
    environment_token_file: Option<OsString>,
    direct_token: Option<OsString>,
    default_token_file: Option<PathBuf>,
) -> Result<SecretToken> {
    let token_file = cli_token_file.or_else(|| environment_token_file.map(PathBuf::from));
    if token_file.is_some() && direct_token.is_some() {
        return Err(Error::Config(
            "provide a token file or ACP_TUNNEL_TOKEN, but not both".into(),
        ));
    }
    if let Some(path) = token_file {
        return read_token_file(&path);
    }
    if let Some(token) = direct_token {
        let token = token.into_string().map_err(|_| {
            Error::Config("ACP_TUNNEL_TOKEN must contain valid Unicode text".into())
        })?;
        return validate_token_text(token);
    }
    if let Some(path) = default_token_file {
        return read_token_file(&path);
    }
    Err(Error::Config(
        "set ACP_TUNNEL_TOKEN_FILE or ACP_TUNNEL_TOKEN, or use --token-file".into(),
    ))
}

fn default_token_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME");
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE"));
    home.filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_TOKEN_FILE))
}

fn read_token_file(path: &Path) -> Result<SecretToken> {
    let mut file = File::open(path)
        .map_err(|error| Error::Config(format!("cannot read {}: {error}", path.display())))?;
    warn_if_token_file_is_readable_by_others(path, &file)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Config(format!("cannot read {}: {error}", path.display())))?;
    if bytes.len() as u64 > MAX_TOKEN_FILE_BYTES {
        return Err(Error::Config(format!(
            "token file {} exceeds 16 KiB",
            path.display()
        )));
    }
    let token = String::from_utf8(bytes).map_err(|_| {
        Error::Config(format!(
            "token file {} must contain valid Unicode text",
            path.display()
        ))
    })?;
    validate_token_text(token)
}

fn validate_token_text(mut token: String) -> Result<SecretToken> {
    if token.ends_with("\r\n") {
        token.truncate(token.len().saturating_sub(2));
    } else if token.ends_with('\n') {
        token.pop();
    }
    if token.contains(['\r', '\n']) {
        return Err(Error::Config(
            "tunnel credential must not contain embedded newlines".into(),
        ));
    }
    SecretToken::new(token)
}

#[cfg(unix)]
fn warn_if_token_file_is_readable_by_others(path: &Path, file: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mode = file
        .metadata()
        .map_err(|error| Error::Config(format!("cannot inspect {}: {error}", path.display())))?
        .mode();
    if mode & 0o044 != 0 {
        eprintln!(
            "acp-tunnel: warning: token file {} is group-readable or world-readable",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn warn_if_token_file_is_readable_by_others(_path: &Path, _file: &File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use tempfile::NamedTempFile;

    use super::*;

    fn file_with(contents: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, contents).unwrap();
        file
    }

    #[test]
    fn direct_environment_token_is_loaded() {
        let token =
            load_token_from_sources(None, None, Some(OsString::from("direct")), None).unwrap();
        assert_eq!(token.expose(), "direct");
    }

    #[test]
    fn cli_token_file_takes_precedence_over_environment_file() {
        let cli = file_with(b"cli\n");
        let environment = file_with(b"environment\n");
        let token = load_token_from_sources(
            Some(cli.path().to_path_buf()),
            Some(environment.path().as_os_str().to_owned()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(token.expose(), "cli");
    }

    #[test]
    fn environment_token_file_is_loaded() {
        let file = file_with(b"file-token");
        let token =
            load_token_from_sources(None, Some(file.path().as_os_str().to_owned()), None, None)
                .unwrap();
        assert_eq!(token.expose(), "file-token");
    }

    #[test]
    fn direct_token_and_token_file_are_rejected() {
        let file = file_with(b"file-token");
        let error = load_token_from_sources(
            Some(file.path().to_path_buf()),
            None,
            Some(OsString::from("direct-secret")),
            None,
        )
        .unwrap_err();
        assert!(!error.to_string().contains("direct-secret"));
    }

    #[test]
    fn missing_and_empty_tokens_are_rejected() {
        assert!(load_token_from_sources(None, None, None, None).is_err());
        assert!(load_token_from_sources(None, None, Some(OsString::new()), None).is_err());
        let file = file_with(b"\n");
        assert!(
            load_token_from_sources(Some(file.path().to_path_buf()), None, None, None).is_err()
        );
    }

    #[test]
    fn one_lf_or_crlf_is_removed() {
        for contents in [b"secret\n".as_slice(), b"secret\r\n".as_slice()] {
            let file = file_with(contents);
            let token =
                load_token_from_sources(Some(file.path().to_path_buf()), None, None, None).unwrap();
            assert_eq!(token.expose(), "secret");
        }
    }

    #[test]
    fn spaces_are_preserved() {
        let file = file_with(b"  secret value  \n");
        let token =
            load_token_from_sources(Some(file.path().to_path_buf()), None, None, None).unwrap();
        assert_eq!(token.expose(), "  secret value  ");
    }

    #[test]
    fn embedded_newlines_are_rejected_without_disclosing_the_token() {
        let file = file_with(b"first-secret\nsecond-secret\n");
        let error =
            load_token_from_sources(Some(file.path().to_path_buf()), None, None, None).unwrap_err();
        let formatted = error.to_string();
        assert!(!formatted.contains("first-secret"));
        assert!(!formatted.contains("second-secret"));
    }

    #[test]
    fn oversized_file_is_rejected() {
        let file = file_with(&vec![b'x'; (MAX_TOKEN_FILE_BYTES + 1) as usize]);
        assert!(
            load_token_from_sources(Some(file.path().to_path_buf()), None, None, None).is_err()
        );
    }

    #[test]
    fn default_token_file_is_the_last_fallback() {
        let file = file_with(b"default-token\n");
        let token =
            load_token_from_sources(None, None, None, Some(file.path().to_path_buf())).unwrap();
        assert_eq!(token.expose(), "default-token");

        let direct = load_token_from_sources(
            None,
            None,
            Some(OsString::from("direct-token")),
            Some(file.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(direct.expose(), "direct-token");
    }

    #[test]
    fn debug_is_redacted() {
        let token = SecretToken::new("debug-secret".into()).unwrap();
        let formatted = format!("{token:?}");
        assert!(!formatted.contains("debug-secret"));
        assert!(formatted.contains("REDACTED"));
    }
}
