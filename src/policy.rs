use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use agent_client_protocol::schema::v2::{EnvVariable, McpServer, McpServerStdio};
use serde_json::{Map, Value, json};

use crate::{
    Error, Result,
    config::{McpPolicy, McpServerConfig, validate_environment_name},
    process::selected_mcp_environment,
};

const SESSION_NEW_METHOD: &str = "session/new";
const LIFECYCLE_METHODS: [&str; 3] = [SESSION_NEW_METHOD, "session/load", "session/resume"];

/// Outcome of applying configured ACP path and MCP policies.
#[derive(Clone, Eq, PartialEq)]
pub enum PolicyOutcome {
    /// Forward this line to the agent.
    Forward(String),
    /// Send this correlated JSON-RPC error to the client instead.
    Reject(String),
}

impl fmt::Debug for PolicyOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(_) => formatter.write_str("Forward([REDACTED ACP PAYLOAD])"),
            Self::Reject(_) => formatter.write_str("Reject([REDACTED JSON-RPC ERROR])"),
        }
    }
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
                    let environment = merge_mcp_environment(
                        selected_mcp_environment(configured),
                        &configured.client_env_allowlist,
                        server.get("env"),
                    )?;
                    replacements.push(configured_mcp(name, configured, environment).map_err(
                        |error| format!("cannot construct MCP server {name:?}: {error}"),
                    )?);
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
    environment: BTreeMap<String, String>,
) -> std::result::Result<Value, serde_json::Error> {
    let environment = environment
        .into_iter()
        .map(|(name, value)| EnvVariable::new(name, value))
        .collect();
    serde_json::to_value(McpServer::Stdio(
        McpServerStdio::new(name, configured.command.clone())
            .args(configured.args.clone())
            .env(environment),
    ))
}

fn merge_mcp_environment(
    mut server_environment: BTreeMap<String, String>,
    client_allowlist: &BTreeSet<String>,
    incoming: Option<&Value>,
) -> std::result::Result<BTreeMap<String, String>, String> {
    let client_environment = parse_client_environment(incoming)?;
    for (name, value) in client_environment {
        if client_allowlist.contains(&name) {
            server_environment.entry(name).or_insert(value);
        }
    }
    Ok(server_environment)
}

fn parse_client_environment(
    incoming: Option<&Value>,
) -> std::result::Result<BTreeMap<String, String>, String> {
    let Some(incoming) = incoming else {
        return Ok(BTreeMap::new());
    };
    let mut parsed = BTreeMap::new();
    match incoming {
        Value::Array(entries) => {
            for entry in entries {
                let object = entry
                    .as_object()
                    .ok_or_else(|| "each MCP environment entry must be an object".to_owned())?;
                let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
                    "each MCP environment entry must have a string name".to_owned()
                })?;
                let value = object.get("value").and_then(Value::as_str).ok_or_else(|| {
                    "each MCP environment entry must have a string value".to_owned()
                })?;
                validate_client_environment_name(name)?;
                if parsed.insert(name.to_owned(), value.to_owned()).is_some() {
                    return Err("MCP environment contains a duplicate variable name".into());
                }
            }
        }
        Value::Object(entries) => {
            for (name, value) in entries {
                validate_client_environment_name(name)?;
                let value = value
                    .as_str()
                    .ok_or_else(|| "each MCP environment value must be a string".to_owned())?;
                parsed.insert(name.clone(), value.to_owned());
            }
        }
        _ => return Err("MCP server env must be an object or an array".into()),
    }
    Ok(parsed)
}

fn validate_client_environment_name(name: &str) -> std::result::Result<(), String> {
    validate_environment_name("client MCP server", "request", name)
        .map_err(|_| "MCP environment contains an invalid variable name".into())
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
                client_env_allowlist: BTreeSet::new(),
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
    fn environment_merge_keeps_only_allowed_client_values_with_server_precedence() {
        let server = BTreeMap::from([
            ("FIXED".into(), "server-fixed".into()),
            ("PASSED".into(), "server-selected".into()),
        ]);
        let allowlist = BTreeSet::from(["CLIENT".into(), "FIXED".into(), "PASSED".into()]);
        let incoming = json!([
            {"name":"CLIENT","value":"client-secret"},
            {"name":"UNLISTED","value":"discarded-secret"},
            {"name":"FIXED","value":"client-fixed"},
            {"name":"PASSED","value":"client-passed"}
        ]);
        let merged = merge_mcp_environment(server, &allowlist, Some(&incoming)).unwrap();
        assert_eq!(
            merged.get("CLIENT").map(String::as_str),
            Some("client-secret")
        );
        assert_eq!(
            merged.get("FIXED").map(String::as_str),
            Some("server-fixed")
        );
        assert_eq!(
            merged.get("PASSED").map(String::as_str),
            Some("server-selected")
        );
        assert!(!merged.contains_key("UNLISTED"));
    }

    #[test]
    fn no_client_allowlist_preserves_server_environment() {
        let server = BTreeMap::from([("FIXED".into(), "server".into())]);
        let incoming = json!({"CLIENT":"secret"});
        assert_eq!(
            merge_mcp_environment(server.clone(), &BTreeSet::new(), Some(&incoming)).unwrap(),
            server
        );
    }

    #[test]
    fn stable_object_environment_is_accepted() {
        let incoming = json!({"CLIENT":"secret"});
        let merged = merge_mcp_environment(
            BTreeMap::new(),
            &BTreeSet::from(["CLIENT".into()]),
            Some(&incoming),
        )
        .unwrap();
        assert_eq!(merged.get("CLIENT").map(String::as_str), Some("secret"));
    }

    #[test]
    fn duplicate_and_malformed_client_environment_are_rejected_without_values() {
        for incoming in [
            json!([
                {"name":"CLIENT","value":"first-secret"},
                {"name":"CLIENT","value":"second-secret"}
            ]),
            json!([{"name":"CLIENT","value":7}]),
            json!([{"name":"BAD=NAME","value":"third-secret"}]),
        ] {
            let error = merge_mcp_environment(
                BTreeMap::new(),
                &BTreeSet::from(["CLIENT".into()]),
                Some(&incoming),
            )
            .unwrap_err();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn rewriting_preserves_request_extensions_and_redacts_debug() {
        let mut policy = policy(true, McpPolicy::Allowlisted);
        policy
            .mcp_servers
            .get_mut("tools")
            .unwrap()
            .client_env_allowlist
            .insert("CLIENT".into());
        let line = r#"{"jsonrpc":"2.0","id":"x","method":"session/new","params":{"cwd":"/x","mcpServers":[{"name":"tools","command":"evil","args":["bad"],"env":{"CLIENT":"client-debug-secret"},"cwd":"/client"}],"_meta":{"kept":true},"future":"kept"},"vendor":"kept"}"#;
        let outcome = policy.apply(line).unwrap();
        let debug = format!("{outcome:?}");
        assert!(!debug.contains("client-debug-secret"));
        let output = forwarded(outcome);
        let server = &output["params"]["mcpServers"][0];
        assert_eq!(server["command"], "/server/tools");
        assert_eq!(server["args"], json!(["serve"]));
        assert_eq!(
            server["env"],
            json!([
                {"name":"CLIENT","value":"client-debug-secret"},
                {"name":"FIXED","value":"yes"}
            ])
        );
        assert!(server.get("cwd").is_none());
        assert_eq!(output["params"]["_meta"]["kept"], true);
        assert_eq!(output["params"]["future"], "kept");
        assert_eq!(output["vendor"], "kept");
    }

    #[test]
    fn absent_client_environment_uses_only_server_environment() {
        let line = r#"{"id":1,"method":"session/new","params":{"mcpServers":[{"name":"tools"}]}}"#;
        let output = forwarded(policy(true, McpPolicy::Allowlisted).apply(line).unwrap());
        assert_eq!(
            output["params"]["mcpServers"][0]["env"],
            json!([{"name":"FIXED","value":"yes"}])
        );
    }

    #[test]
    fn malformed_environment_returns_a_correlated_error_without_secret_values() {
        let line = r#"{"jsonrpc":"2.0","id":"request-env","method":"session/new","params":{"mcpServers":[{"name":"tools","env":[{"name":"CLIENT","value":"do-not-disclose"},{"name":"CLIENT","value":"also-secret"}]}]}}"#;
        let PolicyOutcome::Reject(error) =
            policy(true, McpPolicy::Allowlisted).apply(line).unwrap()
        else {
            panic!("expected rejection");
        };
        assert!(!error.contains("do-not-disclose"));
        assert!(!error.contains("also-secret"));
        let error: Value = serde_json::from_str(&error).unwrap();
        assert_eq!(error["id"], "request-env");
        assert_eq!(error["error"]["code"], -32001);
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
