//! Server initialization, diagnostics, and service-file generation.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Write,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::{
    Error, Result,
    client::validate_connect_url,
    config::{McpPolicy, ServerConfig, validate_environment_name, validate_id},
    credentials::{load_token, load_token_file},
    process::selected_environment,
};

const BUZZ_CLIENT_ENVIRONMENT: [&str; 3] = ["BUZZ_RELAY_URL", "BUZZ_PRIVATE_KEY", "BUZZ_AUTH_TAG"];

/// Values used to create one server configuration.
#[derive(Debug)]
pub struct InitOptions {
    /// Output configuration path.
    pub config_path: PathBuf,
    /// Token path created when no token exists.
    pub token_path: PathBuf,
    /// Public agent identifier.
    pub agent_id: String,
    /// Agent executable path or name.
    pub command: PathBuf,
    /// Public workspace identifier.
    pub workspace_id: String,
    /// Existing workspace directory.
    pub workspace_path: PathBuf,
    /// Server environment names inherited by the agent.
    pub pass_env: BTreeSet<String>,
    /// Client environment names accepted by the agent.
    pub client_env_allowlist: BTreeSet<String>,
    /// Initial MCP policy.
    pub mcp_policy: McpPolicy,
    /// Replace an existing configuration file.
    pub force: bool,
}

/// Files and identifiers created by server initialization.
#[derive(Debug, Eq, PartialEq)]
pub struct InitReport {
    /// Written configuration path.
    pub config_path: PathBuf,
    /// Existing or newly created token path.
    pub token_path: PathBuf,
    /// True when initialization created the token.
    pub token_created: bool,
    /// Resolved absolute agent executable.
    pub command: PathBuf,
    /// Canonical workspace path.
    pub workspace_path: PathBuf,
}

#[derive(Serialize)]
struct GeneratedConfig {
    allow_insecure_mcp_passthrough: bool,
    agents: BTreeMap<String, GeneratedAgent>,
    workspaces: BTreeMap<String, GeneratedWorkspace>,
}

#[derive(Serialize)]
struct GeneratedAgent {
    command: PathBuf,
    workspaces: BTreeSet<String>,
    pass_env: BTreeSet<String>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    client_env_allowlist: BTreeSet<String>,
    mcp_policy: &'static str,
}

#[derive(Serialize)]
struct GeneratedWorkspace {
    path: PathBuf,
}

/// Creates and validates a server configuration and token file.
pub fn initialize_server(options: InitOptions) -> Result<InitReport> {
    validate_id("agent", &options.agent_id)?;
    validate_id("workspace", &options.workspace_id)?;
    for name in options.pass_env.iter().chain(&options.client_env_allowlist) {
        validate_environment_name("generated agent", &options.agent_id, name)?;
    }

    let command = resolve_executable(&options.command, std::env::var_os("PATH").as_deref())?;
    let workspace_path = fs::canonicalize(&options.workspace_path).map_err(|error| {
        Error::Config(format!(
            "cannot resolve workspace {}: {error}",
            options.workspace_path.display()
        ))
    })?;
    if !workspace_path.is_dir() {
        return Err(Error::Config(format!(
            "workspace {} is not a directory",
            workspace_path.display()
        )));
    }
    if let Ok(metadata) = fs::symlink_metadata(&options.config_path) {
        if metadata.file_type().is_symlink() {
            return Err(Error::Config(format!(
                "configuration {} must not be a symbolic link",
                options.config_path.display()
            )));
        }
        if !options.force {
            return Err(Error::Config(format!(
                "configuration {} already exists; use --force to replace it",
                options.config_path.display()
            )));
        }
    }

    let token_created = if options.token_path.exists() {
        let _ = load_token_file(&options.token_path)?;
        false
    } else {
        let token = format!("{}{}\n", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        write_private_file(&options.token_path, token.as_bytes())?;
        true
    };

    let policy = match options.mcp_policy {
        McpPolicy::Deny => "deny",
        McpPolicy::Allowlisted => "allowlisted",
        McpPolicy::Passthrough => "passthrough",
    };
    let generated = GeneratedConfig {
        allow_insecure_mcp_passthrough: options.mcp_policy == McpPolicy::Passthrough,
        agents: BTreeMap::from([(
            options.agent_id,
            GeneratedAgent {
                command: command.clone(),
                workspaces: BTreeSet::from([options.workspace_id.clone()]),
                pass_env: options.pass_env,
                client_env_allowlist: options.client_env_allowlist,
                mcp_policy: policy,
            },
        )]),
        workspaces: BTreeMap::from([(
            options.workspace_id,
            GeneratedWorkspace {
                path: workspace_path.clone(),
            },
        )]),
    };
    let text = toml::to_string_pretty(&generated).map_err(|error| {
        Error::Config(format!("cannot serialize generated configuration: {error}"))
    })?;
    let generated_config: ServerConfig = toml::from_str(&text)?;
    generated_config.validate()?;
    if options.force {
        replace_private_file(&options.config_path, text.as_bytes())?;
    } else {
        write_private_file(&options.config_path, text.as_bytes())?;
    }
    let _ = ServerConfig::load(&options.config_path)?;

    Ok(InitReport {
        config_path: options.config_path,
        token_path: options.token_path,
        token_created,
        command,
        workspace_path,
    })
}

/// Result level for one server diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorLevel {
    /// The diagnostic succeeded.
    Ok,
    /// The diagnostic found a risk or an inconclusive state.
    Warning,
    /// The diagnostic found an error that prevents correct operation.
    Error,
}

/// One server diagnostic result.
#[derive(Debug, Eq, PartialEq)]
pub struct DoctorNotice {
    /// Result severity.
    pub level: DoctorLevel,
    /// Human-readable result without secret values.
    pub message: String,
}

/// Complete server diagnostic report.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct DoctorReport {
    /// Ordered diagnostic results.
    pub notices: Vec<DoctorNotice>,
}

impl DoctorReport {
    /// Returns true when one or more diagnostics failed.
    pub fn has_errors(&self) -> bool {
        self.notices
            .iter()
            .any(|notice| notice.level == DoctorLevel::Error)
    }

    /// Appends all diagnostics from another report.
    pub fn append(&mut self, mut other: Self) {
        self.notices.append(&mut other.notices);
    }

    fn push(&mut self, level: DoctorLevel, message: impl Into<String>) {
        self.notices.push(DoctorNotice {
            level,
            message: message.into(),
        });
    }
}

/// Examines whether a public URL reaches an authenticated WebSocket route.
pub async fn diagnose_websocket_endpoint(url: &Url) -> DoctorReport {
    let mut report = DoctorReport::default();
    if let Err(error) = validate_connect_url(url) {
        report.push(DoctorLevel::Error, error.to_string());
        return report;
    }
    match tokio_tungstenite::connect_async(url.as_str()).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status() == http::StatusCode::UNAUTHORIZED =>
        {
            report.push(
                DoctorLevel::Ok,
                "the public WebSocket route rejected an unauthenticated request",
            );
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => report.push(
            DoctorLevel::Error,
            format!(
                "the public WebSocket route returned HTTP {}",
                response.status()
            ),
        ),
        Err(error) => report.push(
            DoctorLevel::Error,
            format!("the public WebSocket handshake failed: {error}"),
        ),
        Ok(_) => report.push(
            DoctorLevel::Error,
            "the public WebSocket route accepted an unauthenticated request",
        ),
    }
    report
}

/// Examines server files, commands, workspaces, listener state, and an optional public URL.
pub fn diagnose_server(
    config_path: &Path,
    token_file: Option<&Path>,
    public_url: Option<&Url>,
) -> DoctorReport {
    let mut report = DoctorReport::default();
    let config = match ServerConfig::load(config_path) {
        Ok(config) => {
            report.push(
                DoctorLevel::Ok,
                format!("configuration {} is valid", config_path.display()),
            );
            config
        }
        Err(error) => {
            report.push(DoctorLevel::Error, error.to_string());
            return report;
        }
    };

    match load_token(token_file) {
        Ok(_) => report.push(DoctorLevel::Ok, "the bearer credential is valid"),
        Err(error) => report.push(DoctorLevel::Error, error.to_string()),
    }

    for (id, workspace) in &config.workspaces {
        match fs::metadata(&workspace.path) {
            Ok(metadata) if metadata.is_dir() => {
                report.push(
                    DoctorLevel::Ok,
                    format!("workspace {id:?} exists at {}", workspace.path.display()),
                );
                if !has_write_permission(&metadata) {
                    report.push(
                        DoctorLevel::Warning,
                        format!("workspace {id:?} has no filesystem write bits"),
                    );
                }
                if let Err(error) = fs::read_dir(&workspace.path) {
                    report.push(
                        DoctorLevel::Error,
                        format!("cannot read workspace {id:?}: {error}"),
                    );
                }
            }
            Ok(_) => report.push(
                DoctorLevel::Error,
                format!("workspace {id:?} is not a directory"),
            ),
            Err(error) => report.push(
                DoctorLevel::Error,
                format!("cannot access workspace {id:?}: {error}"),
            ),
        }
    }

    for (id, agent) in &config.agents {
        let environment = selected_environment(&agent.pass_env, &agent.env);
        for name in &agent.pass_env {
            if !environment.contains_key(name) {
                report.push(
                    DoctorLevel::Warning,
                    format!("agent {id:?} cannot inherit unset variable {name:?}"),
                );
            }
        }
        let search_path = environment.get("PATH").map(OsStr::new);
        match resolve_executable(&agent.command, search_path) {
            Ok(command) => report.push(
                DoctorLevel::Ok,
                format!("agent {id:?} executable is {}", command.display()),
            ),
            Err(error) => report.push(DoctorLevel::Error, error.to_string()),
        }
        if agent.mcp_policy == McpPolicy::Passthrough {
            report.push(
                DoctorLevel::Warning,
                format!("agent {id:?} accepts client-provided MCP commands for remote execution"),
            );
        }
        let buzz_names = BUZZ_CLIENT_ENVIRONMENT
            .iter()
            .filter(|name| agent.client_env_allowlist.contains(**name))
            .count();
        if buzz_names == BUZZ_CLIENT_ENVIRONMENT.len() {
            report.push(
                DoctorLevel::Ok,
                format!("agent {id:?} accepts the complete Buzz environment preset"),
            );
        } else if buzz_names != 0 {
            report.push(
                DoctorLevel::Warning,
                format!("agent {id:?} has an incomplete Buzz environment preset"),
            );
        }
    }

    if let Some(tls) = &config.tls {
        for (kind, path) in [
            ("TLS certificate", &tls.cert_path),
            ("TLS private key", &tls.key_path),
        ] {
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => report.push(
                    DoctorLevel::Ok,
                    format!("{kind} exists at {}", path.display()),
                ),
                Ok(_) => report.push(DoctorLevel::Error, format!("{kind} is not a file")),
                Err(error) => report.push(
                    DoctorLevel::Error,
                    format!("cannot access {kind} {}: {error}", path.display()),
                ),
            }
        }
    }

    match TcpListener::bind(config.listen) {
        Ok(listener) => {
            drop(listener);
            report.push(
                DoctorLevel::Ok,
                format!("listener {} is available", config.listen),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => report.push(
            DoctorLevel::Warning,
            format!(
                "listener {} is in use; an acp-tunnel server can already be running",
                config.listen
            ),
        ),
        Err(error) => report.push(
            DoctorLevel::Error,
            format!("cannot bind listener {}: {error}", config.listen),
        ),
    }

    if let Some(url) = public_url {
        diagnose_public_url(url, &mut report);
    }
    report
}

fn diagnose_public_url(url: &Url, report: &mut DoctorReport) {
    if let Err(error) = validate_connect_url(url) {
        report.push(DoctorLevel::Error, error.to_string());
        return;
    }
    let addresses = match url.socket_addrs(|| match url.scheme() {
        "wss" => Some(443),
        "ws" => Some(80),
        _ => None,
    }) {
        Ok(addresses) if !addresses.is_empty() => addresses,
        Ok(_) => {
            report.push(DoctorLevel::Error, "the public URL resolved no addresses");
            return;
        }
        Err(error) => {
            report.push(
                DoctorLevel::Error,
                format!("cannot resolve the public URL: {error}"),
            );
            return;
        }
    };
    if addresses
        .iter()
        .any(|address| TcpStream::connect_timeout(address, Duration::from_secs(2)).is_ok())
    {
        report.push(
            DoctorLevel::Ok,
            "the public URL resolves and accepts a TCP connection",
        );
        report.push(
            DoctorLevel::Warning,
            "the doctor did not open an authenticated WebSocket or start an agent",
        );
    } else {
        report.push(
            DoctorLevel::Error,
            "the public URL did not accept a TCP connection",
        );
    }
}

/// Generates a systemd user-service unit for the current executable.
pub fn generate_user_service(executable: &Path) -> Result<String> {
    let executable = fs::canonicalize(executable).map_err(|error| {
        Error::Config(format!(
            "cannot resolve service executable {}: {error}",
            executable.display()
        ))
    })?;
    let argument = escape_systemd_argument(&executable)?;
    Ok(format!(
        "[Unit]\n\
Description=ACP Tunnel Server\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={argument} serve\n\
Restart=on-failure\n\
RestartSec=2\n\
KillSignal=SIGTERM\n\
TimeoutStopSec=30\n\
KillMode=control-group\n\
NoNewPrivileges=true\n\
PrivateTmp=true\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    ))
}

fn escape_systemd_argument(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        Error::Config("the service executable path must contain valid Unicode text".into())
    })?;
    if text.contains(['\n', '\r', '\0']) {
        return Err(Error::Config(
            "the service executable path contains invalid characters".into(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

fn resolve_executable(command: &Path, search_path: Option<&OsStr>) -> Result<PathBuf> {
    let has_directory = command.is_absolute() || command.components().count() > 1;
    if has_directory {
        return canonical_executable(command);
    }
    let search_path = search_path.ok_or_else(|| {
        Error::Config(format!(
            "cannot resolve executable {} because PATH is unavailable",
            command.display()
        ))
    })?;
    for directory in std::env::split_paths(search_path) {
        let candidate = directory.join(command);
        if is_executable(&candidate) {
            return fs::canonicalize(&candidate).map_err(|error| {
                Error::Config(format!(
                    "cannot resolve executable {}: {error}",
                    candidate.display()
                ))
            });
        }
    }
    Err(Error::Config(format!(
        "cannot find executable {} in PATH",
        command.display()
    )))
}

fn canonical_executable(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        Error::Config(format!(
            "cannot resolve executable {}: {error}",
            path.display()
        ))
    })?;
    if is_executable(&canonical) {
        Ok(canonical)
    } else {
        Err(Error::Config(format!(
            "configured executable {} is not an executable file",
            canonical.display()
        )))
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
fn has_write_permission(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o222 != 0
}

#[cfg(not(unix))]
fn has_write_permission(metadata: &fs::Metadata) -> bool {
    !metadata.permissions().readonly()
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config(format!("path {} has no parent directory", path.display())))?;
    let parent_exists = parent.exists();
    fs::create_dir_all(parent).map_err(|error| {
        Error::Config(format!(
            "cannot create directory {}: {error}",
            parent.display()
        ))
    })?;
    if !parent_exists {
        set_private_directory_permissions(parent)?;
    }

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| Error::Config(format!("cannot write {}: {error}", path.display())))?;
    file.write_all(contents)
        .map_err(|error| Error::Config(format!("cannot write {}: {error}", path.display())))?;
    set_private_file_permissions(path)?;
    Ok(())
}

fn replace_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    set_private_file_permissions(path)?;
    let temporary = sibling_path(path, "new")?;
    let backup = sibling_path(path, "old")?;
    write_private_file(&temporary, contents)?;
    if let Err(error) = fs::rename(path, &backup) {
        let _cleanup_result = fs::remove_file(&temporary);
        return Err(Error::Config(format!(
            "cannot preserve {} before replacement: {error}",
            path.display()
        )));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let restore = fs::rename(&backup, path);
        let _cleanup_result = fs::remove_file(&temporary);
        return match restore {
            Ok(()) => Err(Error::Config(format!(
                "cannot replace {}: {error}. The original file was restored",
                path.display()
            ))),
            Err(restore_error) => Err(Error::Config(format!(
                "cannot replace {}: {error}. Cannot restore original file: {restore_error}",
                path.display()
            ))),
        };
    }
    fs::remove_file(&backup).map_err(|error| {
        Error::Config(format!(
            "configuration was replaced, but backup {} cannot be removed: {error}",
            backup.display()
        ))
    })?;
    Ok(())
}

fn sibling_path(path: &Path, kind: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config(format!("path {} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Config(format!("path {} has no file name", path.display())))?;
    Ok(parent.join(format!(
        ".{}.{}-{}",
        name.to_string_lossy(),
        kind,
        Uuid::new_v4().simple()
    )))
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        Error::Config(format!(
            "cannot set permissions on {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        Error::Config(format!(
            "cannot set permissions on {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable_file(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join("agent");
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn initialization_creates_a_valid_passthrough_buzz_configuration() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let command = executable_file(root.path());
        let config_path = root.path().join("config/config.toml");
        let token_path = root.path().join("config/token");
        let report = initialize_server(InitOptions {
            config_path: config_path.clone(),
            token_path: token_path.clone(),
            agent_id: "codex".into(),
            command: command.clone(),
            workspace_id: "project".into(),
            workspace_path: workspace.clone(),
            pass_env: BTreeSet::from(["HOME".into(), "PATH".into()]),
            client_env_allowlist: BTreeSet::from([
                "BUZZ_RELAY_URL".into(),
                "BUZZ_PRIVATE_KEY".into(),
                "BUZZ_AUTH_TAG".into(),
            ]),
            mcp_policy: McpPolicy::Passthrough,
            force: false,
        })
        .unwrap();

        assert!(report.token_created);
        let config = ServerConfig::load(&config_path).unwrap();
        assert!(config.allow_insecure_mcp_passthrough);
        assert_eq!(config.agents["codex"].mcp_policy, McpPolicy::Passthrough);
        assert!(
            config.agents["codex"]
                .client_env_allowlist
                .contains("BUZZ_PRIVATE_KEY")
        );
        assert_eq!(
            fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(config_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let token_before = fs::read(&token_path).unwrap();
        let replacement = initialize_server(InitOptions {
            config_path,
            token_path: token_path.clone(),
            agent_id: "codex".into(),
            command,
            workspace_id: "project".into(),
            workspace_path: workspace,
            pass_env: BTreeSet::new(),
            client_env_allowlist: BTreeSet::new(),
            mcp_policy: McpPolicy::Allowlisted,
            force: true,
        })
        .unwrap();
        assert!(!replacement.token_created);
        assert_eq!(fs::read(token_path).unwrap(), token_before);
    }

    #[cfg(unix)]
    #[test]
    fn initialization_refuses_to_replace_configuration_without_force() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let config_path = root.path().join("config.toml");
        fs::write(&config_path, "existing").unwrap();
        let error = initialize_server(InitOptions {
            config_path,
            token_path: root.path().join("token"),
            agent_id: "agent".into(),
            command: executable_file(root.path()),
            workspace_id: "workspace".into(),
            workspace_path: workspace,
            pass_env: BTreeSet::new(),
            client_env_allowlist: BTreeSet::new(),
            mcp_policy: McpPolicy::Allowlisted,
            force: false,
        })
        .unwrap_err();
        assert!(error.to_string().contains("use --force"));
    }

    #[cfg(unix)]
    #[test]
    fn initialization_refuses_a_configuration_symlink_with_force() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let target = root.path().join("target.toml");
        fs::write(&target, "preserve").unwrap();
        let config_path = root.path().join("config.toml");
        symlink(&target, &config_path).unwrap();
        let error = initialize_server(InitOptions {
            config_path,
            token_path: root.path().join("token"),
            agent_id: "agent".into(),
            command: executable_file(root.path()),
            workspace_id: "workspace".into(),
            workspace_path: workspace,
            pass_env: BTreeSet::new(),
            client_env_allowlist: BTreeSet::new(),
            mcp_policy: McpPolicy::Passthrough,
            force: true,
        })
        .unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        assert_eq!(fs::read_to_string(target).unwrap(), "preserve");
    }

    #[cfg(unix)]
    #[test]
    fn doctor_and_service_generation_report_expected_state() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let config_path = root.path().join("config.toml");
        let token_path = root.path().join("token");
        initialize_server(InitOptions {
            config_path: config_path.clone(),
            token_path: token_path.clone(),
            agent_id: "agent".into(),
            command: executable_file(root.path()),
            workspace_id: "workspace".into(),
            workspace_path: workspace,
            pass_env: BTreeSet::new(),
            client_env_allowlist: BTreeSet::new(),
            mcp_policy: McpPolicy::Allowlisted,
            force: false,
        })
        .unwrap();
        let report = diagnose_server(&config_path, Some(&token_path), None);
        assert!(!report.has_errors(), "{report:?}");

        let service = generate_user_service(&std::env::current_exe().unwrap()).unwrap();
        assert!(service.contains("ExecStart="));
        assert!(service.contains(" serve\n"));
        assert!(service.contains("WantedBy=default.target"));
    }
}
