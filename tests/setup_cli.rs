#![cfg(unix)]
#![doc = "Infrastructure-free server setup CLI tests."]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

use acp_tunnel::config::ServerConfig;

#[test]
fn init_doctor_and_service_use_the_simple_server_flow() {
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let executable = std::env::var("CARGO_BIN_EXE_acp-tunnel")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_acp-tunnel").to_owned());

    let init = Command::new(&executable)
        .args([
            "init",
            "--agent",
            "codex",
            "--agent-command",
            "/bin/sh",
            "--workspace",
            "project",
            "--workspace-path",
            workspace.to_str().unwrap(),
            "--buzz",
        ])
        .env("HOME", home.path())
        .env_remove("ACP_TUNNEL_TOKEN")
        .env_remove("ACP_TUNNEL_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        String::from_utf8_lossy(&init.stderr).contains("MCP passthrough permits"),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let config_path = home.path().join(".config/acp-tunnel/config.toml");
    let token_path = home.path().join(".config/acp-tunnel/token");
    assert!(config_path.is_file());
    assert!(token_path.is_file());
    let config = ServerConfig::load(&config_path).unwrap();
    assert!(config.allow_insecure_mcp_passthrough);
    assert!(
        config.agents["codex"]
            .client_env_allowlist
            .contains("BUZZ_PRIVATE_KEY")
    );

    let doctor = Command::new(&executable)
        .arg("doctor")
        .env("HOME", home.path())
        .env_remove("ACP_TUNNEL_TOKEN")
        .env_remove("ACP_TUNNEL_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor_stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(doctor_stdout.contains("complete Buzz environment preset"));
    assert!(doctor_stdout.contains("client-provided MCP commands"));

    let service = Command::new(&executable)
        .args(["service", "generate", "--user"])
        .output()
        .unwrap();
    assert!(service.status.success());
    let service_stdout = String::from_utf8_lossy(&service.stdout);
    assert!(service_stdout.contains("ExecStart="));
    assert!(service_stdout.contains(" serve\n"));
    assert!(service_stdout.contains("TimeoutStopSec=30"));
}
