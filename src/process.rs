use std::{
    collections::BTreeMap,
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use tokio::{
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    time::timeout,
};

use crate::{
    Error, Result,
    config::{AgentConfig, McpServerConfig, validate_environment_name},
    protocol::ClientEnvironment,
};

/// The piped handles and identity of one running ACP agent.
pub struct AgentProcess {
    child: Child,
    /// Agent standard input.
    stdin: Option<ChildStdin>,
    /// Agent standard output.
    stdout: Option<ChildStdout>,
    /// Agent standard error.
    stderr: Option<ChildStderr>,
    /// Child process identifier.
    pub pid: u32,
}

impl AgentProcess {
    /// Spawns one configured ACP agent without invoking a shell.
    pub fn spawn(
        config: &AgentConfig,
        workspace: &Path,
        client_environment: &ClientEnvironment,
    ) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        command.env_clear();
        command.envs(merge_agent_environment(config, client_environment)?);
        configure_process_group(&mut command);

        let mut child = command.spawn().map_err(|error| {
            Error::Process(format!(
                "cannot start configured agent executable {}: {error}",
                config.command.display()
            ))
        })?;
        let pid = child
            .id()
            .ok_or_else(|| Error::Process("spawned agent has no process ID".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Process("agent stdin pipe is unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Process("agent stdout pipe is unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Process("agent stderr pipe is unavailable".into()))?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            pid,
        })
    }

    /// Takes ownership of the agent standard-input pipe.
    pub fn take_stdin(&mut self) -> Result<ChildStdin> {
        self.stdin
            .take()
            .ok_or_else(|| Error::Process("agent stdin pipe was already taken".into()))
    }

    /// Takes ownership of the agent standard-output pipe.
    pub fn take_stdout(&mut self) -> Result<ChildStdout> {
        self.stdout
            .take()
            .ok_or_else(|| Error::Process("agent stdout pipe was already taken".into()))
    }

    /// Takes ownership of the agent standard-error pipe.
    pub fn take_stderr(&mut self) -> Result<ChildStderr> {
        self.stderr
            .take()
            .ok_or_else(|| Error::Process("agent stderr pipe was already taken".into()))
    }

    /// Waits for the child process to terminate.
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        self.child
            .wait()
            .await
            .map_err(|error| Error::Process(format!("cannot wait for agent: {error}")))
    }

    /// Terminates the complete process group and reaps the direct child.
    pub async fn terminate_and_reap(&mut self, shutdown_timeout: Duration) -> Result<ExitStatus> {
        if let Some(status) = self
            .child
            .try_wait()
            .map_err(|error| Error::Process(format!("cannot inspect agent status: {error}")))?
        {
            return Ok(status);
        }

        terminate_group(self.pid, false, &mut self.child)?;
        if let Ok(status) = timeout(shutdown_timeout, self.child.wait()).await {
            let status = status.map_err(|error| {
                Error::Process(format!("cannot reap agent after termination: {error}"))
            })?;
            cleanup_remaining_group(self.pid, shutdown_timeout).await?;
            return Ok(status);
        }

        terminate_group(self.pid, true, &mut self.child)?;
        timeout(shutdown_timeout, self.child.wait())
            .await
            .map_err(|_| Error::Timeout("force-killing remote agent"))?
            .map_err(|error| Error::Process(format!("cannot reap force-killed agent: {error}")))
    }

    /// Waits for a cooperative exit, then escalates from termination to force-kill.
    pub async fn graceful_shutdown_and_reap(
        &mut self,
        graceful_exit_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Result<ExitStatus> {
        if let Some(status) = self
            .child
            .try_wait()
            .map_err(|error| Error::Process(format!("cannot inspect agent status: {error}")))?
        {
            cleanup_remaining_group(self.pid, shutdown_timeout).await?;
            return Ok(status);
        }

        if let Ok(status) = timeout(graceful_exit_timeout, self.child.wait()).await {
            let status = status.map_err(|error| {
                Error::Process(format!("cannot reap agent after stdin closure: {error}"))
            })?;
            cleanup_remaining_group(self.pid, shutdown_timeout).await?;
            return Ok(status);
        }

        terminate_group(self.pid, false, &mut self.child)?;
        if let Ok(status) = timeout(shutdown_timeout, self.child.wait()).await {
            return status.map_err(|error| {
                Error::Process(format!("cannot reap agent after termination: {error}"))
            });
        }

        terminate_group(self.pid, true, &mut self.child)?;
        timeout(shutdown_timeout, self.child.wait())
            .await
            .map_err(|_| Error::Timeout("force-killing remote agent"))?
            .map_err(|error| Error::Process(format!("cannot reap force-killed agent: {error}")))
    }
}

#[cfg(unix)]
async fn cleanup_remaining_group(pid: u32, shutdown_timeout: Duration) -> Result<()> {
    if !process_group_exists(pid)? {
        return Ok(());
    }
    signal_group(pid, false)?;
    if wait_for_process_group_exit(pid, shutdown_timeout).await? {
        return Ok(());
    }
    signal_group(pid, true)?;
    Ok(())
}

#[cfg(not(unix))]
async fn cleanup_remaining_group(_pid: u32, _shutdown_timeout: Duration) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn process_group_exists(pid: u32) -> Result<bool> {
    use nix::{errno::Errno, sys::signal::killpg, unistd::Pid};

    let raw_pid = i32::try_from(pid)
        .map_err(|_| Error::Process("agent process ID is outside the platform range".into()))?;
    match killpg(Pid::from_raw(raw_pid), None) {
        Ok(()) | Err(Errno::EPERM) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(Error::Process(format!(
            "cannot inspect agent process group: {error}"
        ))),
    }
}

#[cfg(unix)]
async fn wait_for_process_group_exit(pid: u32, duration: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if !process_group_exists(pid)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Constructs the environment inherited by an agent without revealing values.
pub fn selected_environment(
    pass_env: &std::collections::BTreeSet<String>,
    fixed: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut selected = BTreeMap::new();
    for name in pass_env {
        if let Ok(value) = std::env::var(name) {
            selected.insert(name.clone(), value);
        }
    }
    selected.extend(fixed.clone());
    selected
}

/// Constructs the environment embedded in an allowlisted MCP definition.
pub fn selected_mcp_environment(config: &McpServerConfig) -> BTreeMap<String, String> {
    selected_environment(&config.pass_env, &config.env)
}

/// Merges server-owned agent environment with explicitly allowlisted client values.
pub fn merge_agent_environment(
    config: &AgentConfig,
    client_environment: &ClientEnvironment,
) -> Result<BTreeMap<String, String>> {
    let mut merged = selected_environment(&config.pass_env, &config.env);
    let mut incoming_names = std::collections::BTreeSet::new();
    for variable in client_environment.variables() {
        validate_environment_name("client agent", "request", variable.name()).map_err(|_| {
            Error::Protocol("client environment has an invalid variable name".into())
        })?;
        if variable.value().contains('\0') {
            return Err(Error::Protocol(
                "client environment has an invalid variable value".into(),
            ));
        }
        if !incoming_names.insert(variable.name()) {
            return Err(Error::Protocol(
                "client environment contains a duplicate variable name".into(),
            ));
        }
        if !config.client_env_allowlist.contains(variable.name()) {
            return Err(Error::Protocol(format!(
                "client environment variable {:?} is not allowlisted for this agent",
                variable.name()
            )));
        }
        merged
            .entry(variable.name().to_owned())
            .or_insert_with(|| variable.value().to_owned());
    }
    Ok(merged)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_group(pid: u32, force: bool, _child: &mut Child) -> Result<()> {
    signal_group(pid, force)
}

#[cfg(unix)]
fn signal_group(pid: u32, force: bool) -> Result<()> {
    use nix::{
        sys::signal::{Signal, killpg},
        unistd::Pid,
    };
    let raw_pid = i32::try_from(pid)
        .map_err(|_| Error::Process("agent process ID is outside the platform range".into()))?;
    let signal = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    match killpg(Pid::from_raw(raw_pid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(Error::Process(format!(
            "cannot signal agent process group: {error}"
        ))),
    }
}

#[cfg(not(unix))]
fn terminate_group(_pid: u32, _force: bool, child: &mut Child) -> Result<()> {
    child
        .start_kill()
        .map_err(|error| Error::Process(format!("cannot terminate agent: {error}")))
}

/// Returns portable exit code and Unix signal details.
pub fn exit_details(status: &ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn environment_contains_only_allowlisted_and_fixed_values() {
        let pass_env = BTreeSet::from(["PATH".to_owned(), "ACP_TUNNEL_MISSING_TEST".to_owned()]);
        let fixed = BTreeMap::from([("NO_BROWSER".to_owned(), "1".to_owned())]);
        let selected = selected_environment(&pass_env, &fixed);
        assert_eq!(selected.get("NO_BROWSER").map(String::as_str), Some("1"));
        assert!(!selected.contains_key("ACP_TUNNEL_MISSING_TEST"));
        assert!(
            selected
                .keys()
                .all(|key| pass_env.contains(key) || key == "NO_BROWSER")
        );
    }

    fn agent_config() -> AgentConfig {
        AgentConfig {
            command: "agent".into(),
            args: Vec::new(),
            workspaces: BTreeSet::new(),
            pass_env: BTreeSet::new(),
            env: BTreeMap::from([("SERVER_FIXED".into(), "server".into())]),
            client_env_allowlist: BTreeSet::from(["CLIENT_VALUE".into(), "SERVER_FIXED".into()]),
            mcp_policy: crate::config::McpPolicy::Deny,
        }
    }

    #[test]
    fn agent_environment_accepts_only_allowlisted_client_values() {
        let config = agent_config();
        let client = ClientEnvironment::new(vec![crate::protocol::ClientEnvironmentVariable::new(
            "CLIENT_VALUE".into(),
            "client".into(),
        )]);
        let merged = merge_agent_environment(&config, &client).unwrap();
        assert_eq!(
            merged.get("CLIENT_VALUE").map(String::as_str),
            Some("client")
        );
        assert_eq!(
            merged.get("SERVER_FIXED").map(String::as_str),
            Some("server")
        );
    }

    #[test]
    fn absent_client_agent_environment_preserves_server_environment() {
        let config = agent_config();
        let merged = merge_agent_environment(&config, &ClientEnvironment::default()).unwrap();
        assert_eq!(
            merged.get("SERVER_FIXED").map(String::as_str),
            Some("server")
        );
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn server_agent_environment_wins_client_collision() {
        let config = agent_config();
        let client = ClientEnvironment::new(vec![crate::protocol::ClientEnvironmentVariable::new(
            "SERVER_FIXED".into(),
            "client-secret".into(),
        )]);
        let merged = merge_agent_environment(&config, &client).unwrap();
        assert_eq!(
            merged.get("SERVER_FIXED").map(String::as_str),
            Some("server")
        );
    }

    #[test]
    fn passed_server_agent_environment_wins_client_collision() {
        let Some(server_path) = std::env::var("PATH").ok() else {
            return;
        };
        let mut config = agent_config();
        config.pass_env.insert("PATH".into());
        config.client_env_allowlist.insert("PATH".into());
        let client = ClientEnvironment::new(vec![crate::protocol::ClientEnvironmentVariable::new(
            "PATH".into(),
            "client-secret".into(),
        )]);
        let merged = merge_agent_environment(&config, &client).unwrap();
        assert_eq!(merged.get("PATH"), Some(&server_path));
    }

    #[test]
    fn agent_environment_rejects_unlisted_duplicate_and_malformed_entries() {
        let config = agent_config();
        let cases = [
            ClientEnvironment::new(vec![crate::protocol::ClientEnvironmentVariable::new(
                "UNLISTED".into(),
                "secret-one".into(),
            )]),
            ClientEnvironment::new(vec![
                crate::protocol::ClientEnvironmentVariable::new(
                    "CLIENT_VALUE".into(),
                    "secret-two".into(),
                ),
                crate::protocol::ClientEnvironmentVariable::new(
                    "CLIENT_VALUE".into(),
                    "secret-three".into(),
                ),
            ]),
            ClientEnvironment::new(vec![crate::protocol::ClientEnvironmentVariable::new(
                "BAD=NAME".into(),
                "secret-four".into(),
            )]),
        ];
        for client in cases {
            let error = merge_agent_environment(&config, &client)
                .unwrap_err()
                .to_string();
            for secret in ["secret-one", "secret-two", "secret-three", "secret-four"] {
                assert!(!error.contains(secret));
            }
        }
    }
}
