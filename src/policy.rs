use std::collections::BTreeMap;

use agent_client_protocol::schema::v2::{EnvVariable, McpServer, McpServerStdio};
use serde_json::{Map, Value, json};

use crate::{
    Error, Result,
    config::{McpPolicy, McpServerConfig},
    process::selected_mcp_environment,
};

const SESSION_NEW_METHOD: &str = "session/new";
const LIFECYCLE_METHODS: [&str; 3] = [SESSION_NEW_METHOD, "session/load", "session/resume"];

/// Outcome of applying configured ACP path and MCP policies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyOutcome {
    /// Forward this line to the agent.
    Forward(String),
    /// Send this correlated JSON-RPC error to the client instead.
    Reject(String),
}

/// Applies the only ACP-aware transformations performed by acp-tunnel.
#[derive(Clone)]
pub struct AcpPolicy {
    workspace_path: String,
    rewrite_cwd: bool,
    mcp_policy: McpPolicy,
    mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl AcpPolicy {
    /// Builds a policy for one selected agent and workspace.
    pub fn new(
        workspace_path: String,
        rewrite_cwd: bool,
        mcp_policy: McpPolicy,
        mcp_servers: BTreeMap<String, McpServerConfig>,
    ) -> Self {
        Self {
            workspace_path,
            rewrite_cwd,
            mcp_policy,
            mcp_servers,
        }
    }

    /// Applies lifecycle path mapping and `session/new` MCP controls.
    ///
    /// Every other ACP message is returned byte-for-byte unchanged.
    pub fn apply(&self, line: &str) -> Result<PolicyOutcome> {
        let Some(method) = sniff_method(line) else {
            return Ok(PolicyOutcome::Forward(line.to_owned()));
        };
        if !LIFECYCLE_METHODS.contains(&method.as_str()) {
            return Ok(PolicyOutcome::Forward(line.to_owned()));
        }
        if !self.rewrite_cwd
            && (method != SESSION_NEW_METHOD || self.mcp_policy == McpPolicy::Passthrough)
        {
            return Ok(PolicyOutcome::Forward(line.to_owned()));
        }

        let mut document: Value = serde_json::from_str(line).map_err(|error| {
            Error::Policy(format!("invalid lifecycle JSON-RPC request: {error}"))
        })?;
        let Some(root) = document.as_object_mut() else {
            return Err(Error::Policy(
                "lifecycle JSON-RPC request must be an object".into(),
            ));
        };
        let params = root
            .entry("params")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(params) = params.as_object_mut() else {
            return Err(Error::Policy(
                "lifecycle JSON-RPC params must be an object".into(),
            ));
        };

        if self.rewrite_cwd {
            params.insert("cwd".into(), Value::String(self.workspace_path.clone()));
        }
        if method == SESSION_NEW_METHOD {
            match self.apply_mcp(params) {
                Ok(()) => {}
                Err(message) => {
                    return Ok(PolicyOutcome::Reject(json_rpc_error(
                        root.get("id").cloned().unwrap_or(Value::Null),
                        -32001,
                        &message,
                    )?));
                }
            }
        }
        Ok(PolicyOutcome::Forward(serde_json::to_string(&document)?))
    }

    fn apply_mcp(&self, params: &mut Map<String, Value>) -> std::result::Result<(), String> {
        match self.mcp_policy {
            McpPolicy::Passthrough => Ok(()),
            McpPolicy::Deny => {
                params.insert("mcpServers".into(), Value::Array(Vec::new()));
                Ok(())
            }
            McpPolicy::Allowlisted => {
                let Some(incoming) = params.get_mut("mcpServers") else {
                    return Ok(());
                };
                let Some(servers) = incoming.as_array() else {
                    return Err("params.mcpServers must be an array".into());
                };
                let mut replacements = Vec::with_capacity(servers.len());
                for server in servers {
                    let name = server
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "each MCP server must have a string name".to_owned())?;
                    let configured = self
                        .mcp_servers
                        .get(name)
                        .ok_or_else(|| format!("MCP server {name:?} is not allowlisted"))?;
                    replacements.push(configured_mcp(name, configured).map_err(|error| {
                        format!("cannot construct MCP server {name:?}: {error}")
                    })?);
                }
                *incoming = Value::Array(replacements);
                Ok(())
            }
        }
    }
}

fn sniff_method(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct MethodOnly {
        method: Option<String>,
    }
    serde_json::from_str::<MethodOnly>(line).ok()?.method
}

fn configured_mcp(
    name: &str,
    configured: &McpServerConfig,
) -> std::result::Result<Value, serde_json::Error> {
    let environment = selected_mcp_environment(configured)
        .into_iter()
        .map(|(name, value)| EnvVariable::new(name, value))
        .collect();
    serde_json::to_value(McpServer::Stdio(
        McpServerStdio::new(name, configured.command.clone())
            .args(configured.args.clone())
            .env(environment),
    ))
}

fn json_rpc_error(id: Value, code: i32, message: &str) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    }))?)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use super::*;

    fn policy(rewrite_cwd: bool, mcp_policy: McpPolicy) -> AcpPolicy {
        let mut servers = BTreeMap::new();
        servers.insert(
            "tools".into(),
            McpServerConfig {
                command: PathBuf::from("/server/tools"),
                args: vec!["serve".into()],
                pass_env: BTreeSet::new(),
                env: BTreeMap::from([("FIXED".into(), "yes".into())]),
            },
        );
        AcpPolicy::new("/remote/project".into(), rewrite_cwd, mcp_policy, servers)
    }

    fn forwarded(outcome: PolicyOutcome) -> Value {
        let PolicyOutcome::Forward(line) = outcome else {
            panic!("expected forward");
        };
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn unknown_extensions_pass_through_byte_for_byte() {
        let line = r#" { "method":"vendor/future","_meta":{"x":1} } "#;
        assert_eq!(
            policy(true, McpPolicy::Allowlisted).apply(line).unwrap(),
            PolicyOutcome::Forward(line.into())
        );
    }

    #[test]
    fn rewrites_only_lifecycle_cwd_and_preserves_unknown_fields() {
        let line = r#"{"jsonrpc":"2.0","id":"x","method":"session/load","params":{"cwd":"/local","sessionId":"s","_meta":{"future":{"a":1}}},"vendor":true}"#;
        let output = forwarded(policy(true, McpPolicy::Allowlisted).apply(line).unwrap());
        assert_eq!(output["id"], "x");
        assert_eq!(output["params"]["cwd"], "/remote/project");
        assert_eq!(output["params"]["_meta"]["future"]["a"], 1);
        assert_eq!(output["vendor"], true);
    }

    #[test]
    fn disabled_rewriting_keeps_exact_bytes() {
        let line = r#" {"method":"session/load","params":{"cwd":"/same"}} "#;
        assert_eq!(
            policy(false, McpPolicy::Passthrough).apply(line).unwrap(),
            PolicyOutcome::Forward(line.into())
        );
    }

    #[test]
    fn deny_removes_mcp_servers() {
        let line = r#"{"id":1,"method":"session/new","params":{"cwd":"/x","mcpServers":[{"name":"bad","command":"evil"}]}}"#;
        let output = forwarded(policy(true, McpPolicy::Deny).apply(line).unwrap());
        assert_eq!(output["params"]["mcpServers"], json!([]));
    }

    #[test]
    fn allowlist_replaces_all_client_controlled_fields() {
        let line = r#"{"id":1,"method":"session/new","params":{"cwd":"/x","mcpServers":[{"name":"tools","command":"evil","args":["bad"],"env":[{"name":"X","value":"bad"}]}]}}"#;
        let output = forwarded(policy(true, McpPolicy::Allowlisted).apply(line).unwrap());
        let server = &output["params"]["mcpServers"][0];
        assert_eq!(server["command"], "/server/tools");
        assert_eq!(server["args"], json!(["serve"]));
        assert_eq!(server["env"], json!([{"name":"FIXED","value":"yes"}]));
    }

    #[test]
    fn unknown_mcp_name_returns_correlated_error() {
        let line = r#"{"jsonrpc":"2.0","id":"request-7","method":"session/new","params":{"cwd":"/x","mcpServers":[{"name":"unknown"}]}}"#;
        let PolicyOutcome::Reject(error) =
            policy(true, McpPolicy::Allowlisted).apply(line).unwrap()
        else {
            panic!("expected rejection");
        };
        let error: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(error["id"], "request-7");
        assert_eq!(error["error"]["code"], -32001);
    }
}
