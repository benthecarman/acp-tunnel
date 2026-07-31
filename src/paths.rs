//! Per-user default paths shared by the client and server commands.

use std::{ffi::OsString, path::PathBuf};

use crate::{Error, Result};

const CONFIG_DIRECTORY: &str = ".config/acp-tunnel";

/// Returns the default per-user configuration directory when a home exists.
pub fn default_config_directory() -> Option<PathBuf> {
    home_directory().map(|home| home.join(CONFIG_DIRECTORY))
}

/// Returns the default server configuration path when a home exists.
pub fn default_server_config_file() -> Option<PathBuf> {
    default_config_directory().map(|directory| directory.join("config.toml"))
}

/// Returns the default bearer-token path when a home exists.
pub fn default_token_file() -> Option<PathBuf> {
    default_config_directory().map(|directory| directory.join("token"))
}

/// Resolves an optional server configuration path against the per-user default.
pub fn resolve_server_config_file(path: Option<PathBuf>) -> Result<PathBuf> {
    path.or_else(default_server_config_file).ok_or_else(|| {
        Error::Config("use --config because the process home directory is unavailable".into())
    })
}

#[cfg(not(windows))]
fn home_directory() -> Option<PathBuf> {
    home_directory_from(std::env::var_os("HOME"))
}

#[cfg(not(windows))]
fn home_directory_from(home: Option<OsString>) -> Option<PathBuf> {
    home.filter(|value| !value.is_empty()).map(PathBuf::from)
}

#[cfg(windows)]
fn home_directory() -> Option<PathBuf> {
    home_directory_from(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

#[cfg(windows)]
fn home_directory_from(home: Option<OsString>, user_profile: Option<OsString>) -> Option<PathBuf> {
    home.or(user_profile)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_home_does_not_produce_a_default_path() {
        #[cfg(not(windows))]
        assert_eq!(home_directory_from(Some(OsString::new())), None);
        #[cfg(windows)]
        assert_eq!(
            home_directory_from(Some(OsString::new()), Some(OsString::new())),
            None
        );
    }
}
