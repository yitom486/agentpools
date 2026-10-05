//! Unified Model Context Protocol (MCP) server configuration.
//!
//! Provides a standardized abstraction for MCP servers across different agent runtimes,
//! including ACP v1/v2 agents and native Codex app-server processes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Transport mechanism for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// Unified MCP server definition usable across ACP and native runtimes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    #[serde(flatten)]
    pub transport: McpTransport,
    /// Tool approval policy, Codex config-file shape
    /// (`default_tools_approval_mode` + per-tool `tools.<name>.approval_mode`).
    /// Only forwarded to Codex app-server entries; ACP values are unaffected.
    #[serde(default)]
    pub approval: McpToolApprovals,
}

/// Per-server tool approval overrides, mirroring the official Codex MCP
/// configuration (`default_tools_approval_mode`, `tools.<tool>.approval_mode`).
/// Supported modes include `auto`, `prompt`, `writes`, `approve`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolApprovals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, String>,
}

impl McpServer {
    /// Create a standard I/O (subprocess) MCP server configuration.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            approval: McpToolApprovals::default(),
        }
    }

    /// Create an HTTP / SSE MCP server configuration.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Http {
                url: url.into(),
                headers: BTreeMap::new(),
            },
            approval: McpToolApprovals::default(),
        }
    }

    /// Default approval behavior for this server's tools
    /// (`auto` | `prompt` | `writes` | `approve`). Forwarded to Codex only.
    pub fn with_default_tools_approval_mode(mut self, mode: impl Into<String>) -> Self {
        self.approval.default_tools_approval_mode = Some(mode.into());
        self
    }

    /// Per-tool approval behavior override (`tools.<tool>.approval_mode`).
    pub fn with_tool_approval_mode(
        mut self,
        tool: impl Into<String>,
        mode: impl Into<String>,
    ) -> Self {
        self.approval.tools.insert(tool.into(), mode.into());
        self
    }

    /// Add a command-line argument for a stdio server.
    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        if let McpTransport::Stdio { args, .. } = &mut self.transport {
            args.push(arg.into());
        }
        self
    }

    /// Add multiple command-line arguments for a stdio server.
    pub fn with_args<I, S>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let McpTransport::Stdio { args, .. } = &mut self.transport {
            args.extend(items.into_iter().map(Into::into));
        }
        self
    }

    /// Add an environment variable for a stdio server.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let McpTransport::Stdio { env, .. } = &mut self.transport {
            env.insert(key.into(), value.into());
        }
        self
    }

    /// Add multiple environment variables for a stdio server.
    pub fn with_envs<I, K, V>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        if let McpTransport::Stdio { env, .. } = &mut self.transport {
            for (k, v) in items {
                env.insert(k.into(), v.into());
            }
        }
        self
    }

    /// Add an HTTP header for an HTTP server.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        if let McpTransport::Http { headers, .. } = &mut self.transport {
            headers.insert(key.into(), value.into());
        }
        self
    }

    /// Add multiple HTTP headers for an HTTP server.
    pub fn with_headers<I, K, V>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        if let McpTransport::Http { headers, .. } = &mut self.transport {
            for (k, v) in items {
                headers.insert(k.into(), v.into());
            }
        }
        self
    }

    /// Converts this MCP server definition into an ACP-compliant JSON object for `session/new.params.mcpServers`.
    pub fn to_acp_value(&self) -> Value {
        match &self.transport {
            McpTransport::Stdio { command, args, env } => {
                let env_list: Vec<Value> = env
                    .iter()
                    .map(|(k, v)| json!({ "name": k, "value": v }))
                    .collect();
                json!({
                    "name": self.name,
                    "command": command,
                    "args": args,
                    "env": env_list,
                })
            }
            McpTransport::Http { url, headers } => {
                let headers_list: Vec<Value> = headers
                    .iter()
                    .map(|(k, v)| json!({ "name": k, "value": v }))
                    .collect();
                json!({
                    "name": self.name,
                    "type": "http",
                    "url": url,
                    "headers": headers_list,
                })
            }
        }
    }

    /// Converts this MCP server definition into a key-value entry for Codex app-server's `thread/start.params.config.mcp_servers`.
    ///
    /// The key is the sanitized server name (with whitespace replaced by `_`), and the value contains the server configuration,
    /// including tool approval policy when configured.
    pub fn to_codex_entry(&self) -> (String, Value) {
        let sanitized_name = self.name.replace(char::is_whitespace, "_");
        let mut value = match &self.transport {
            McpTransport::Stdio { command, args, env } => {
                json!({
                    "command": command,
                    "args": args,
                    "env": env,
                })
            }
            McpTransport::Http { url, headers } => {
                json!({
                    "url": url,
                    "http_headers": headers,
                })
            }
        };
        if let Some(mode) = &self.approval.default_tools_approval_mode {
            value["default_tools_approval_mode"] = json!(mode);
        }
        if !self.approval.tools.is_empty() {
            let mut tools = serde_json::Map::new();
            for (tool, mode) in &self.approval.tools {
                tools.insert(tool.clone(), json!({ "approval_mode": mode }));
            }
            value["tools"] = Value::Object(tools);
        }
        (sanitized_name, value)
    }
}

fn parse_key_value_map(
    value: Option<&Value>,
    field_name: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    let Some(val) = value else {
        return Ok(map);
    };
    if val.is_null() {
        return Ok(map);
    }
    if let Some(obj) = val.as_object() {
        for (k, v) in obj {
            let str_val = v
                .as_str()
                .ok_or_else(|| format!("{field_name} value for key '{k}' must be a string"))?;
            map.insert(k.clone(), str_val.to_owned());
        }
    } else if let Some(arr) = val.as_array() {
        for (i, item) in arr.iter().enumerate() {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{field_name} entry {i} missing 'name' string"))?;
            let value_str = item
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{field_name} entry {i} missing 'value' string"))?;
            map.insert(name.to_owned(), value_str.to_owned());
        }
    } else {
        return Err(format!(
            "{field_name} must be an object map or an array of name-value pairs"
        ));
    }
    Ok(map)
}

fn parse_tool_approvals(obj: &serde_json::Map<String, Value>) -> McpToolApprovals {
    let mut approval = McpToolApprovals::default();
    if let Some(mode) = obj
        .get("default_tools_approval_mode")
        .and_then(Value::as_str)
    {
        approval.default_tools_approval_mode = Some(mode.to_owned());
    }
    if let Some(tools) = obj.get("tools").and_then(Value::as_object) {
        for (tool, spec) in tools {
            if let Some(mode) = spec.get("approval_mode").and_then(Value::as_str) {
                approval.tools.insert(tool.clone(), mode.to_owned());
            }
        }
    }
    approval
}

fn parse_single_mcp_server(
    name: &str,
    obj: &serde_json::Map<String, Value>,
) -> Result<McpServer, String> {
    if name.trim().is_empty() {
        return Err("MCP server name must not be empty".into());
    }
    let transport_type = obj.get("type").and_then(Value::as_str);
    let url = obj.get("url").and_then(Value::as_str);
    let command = obj.get("command").and_then(Value::as_str);

    let is_http = matches!(transport_type, Some("http") | Some("sse")) || url.is_some();
    if is_http {
        let Some(url) = url else {
            return Err(format!(
                "MCP server '{name}' of type http/sse requires a 'url'"
            ));
        };
        let headers_val = obj.get("headers").or_else(|| obj.get("http_headers"));
        let headers = parse_key_value_map(headers_val, &format!("MCP server '{name}' headers"))?;
        Ok(McpServer {
            name: name.to_owned(),
            transport: McpTransport::Http {
                url: url.to_owned(),
                headers,
            },
            approval: parse_tool_approvals(obj),
        })
    } else {
        let Some(command) = command else {
            return Err(format!(
                "MCP server '{name}' must specify either 'command' or 'url'"
            ));
        };
        let args = if let Some(args_val) = obj.get("args") {
            let arr = args_val
                .as_array()
                .ok_or_else(|| format!("MCP server '{name}' args must be an array of strings"))?;
            arr.iter()
                .map(|item| {
                    item.as_str().map(str::to_owned).ok_or_else(|| {
                        format!("MCP server '{name}' args must contain only strings")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let env = parse_key_value_map(obj.get("env"), &format!("MCP server '{name}' env"))?;
        Ok(McpServer {
            name: name.to_owned(),
            transport: McpTransport::Stdio {
                command: command.to_owned(),
                args,
                env,
            },
            approval: parse_tool_approvals(obj),
        })
    }
}

/// Parses an MCP server collection from either an array of server objects or a dictionary of named server objects.
pub fn parse_mcp_servers(value: &Value) -> Result<Vec<McpServer>, String> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if let Some(arr) = value.as_array() {
        let mut servers = Vec::with_capacity(arr.len());
        for (index, item) in arr.iter().enumerate() {
            let obj = item
                .as_object()
                .ok_or_else(|| format!("mcpServers entry at index {index} must be an object"))?;
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("mcpServers entry at index {index} missing 'name'"))?;
            servers.push(parse_single_mcp_server(name, obj)?);
        }
        Ok(servers)
    } else if let Some(obj) = value.as_object() {
        let mut servers = Vec::with_capacity(obj.len());
        for (name, item) in obj {
            let item_obj = item
                .as_object()
                .ok_or_else(|| format!("mcpServers entry '{name}' must be an object"))?;
            servers.push(parse_single_mcp_server(name, item_obj)?);
        }
        Ok(servers)
    } else {
        Err(
            "mcpServers must be an array of server objects or a dictionary of named server objects"
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_approval_config_parses_and_reaches_codex_entry() {
        let json = json!([
            {
                "name": "textbook",
                "type": "http",
                "url": "http://127.0.0.1:9/mcp",
                "default_tools_approval_mode": "approve",
                "tools": {
                    "write_grammar": { "approval_mode": "approve" }
                }
            }
        ]);
        let servers = parse_mcp_servers(&json).unwrap();
        assert_eq!(servers.len(), 1);
        let (name, entry) = servers[0].to_codex_entry();
        assert_eq!(name, "textbook");
        assert_eq!(entry["default_tools_approval_mode"], json!("approve"));
        assert_eq!(
            entry["tools"]["write_grammar"]["approval_mode"],
            json!("approve")
        );
        // No approval configured: keys stay out of the entry.
        let plain = McpServer::http("plain", "http://127.0.0.1:9/mcp");
        let (_, plain_entry) = plain.to_codex_entry();
        assert!(plain_entry.get("default_tools_approval_mode").is_none());
        assert!(plain_entry.get("tools").is_none());
    }

    #[test]
    fn parses_array_of_stdio_and_http_servers() {
        let json = json!([
            {
                "name": "sqlite",
                "command": "uvx",
                "args": ["mcp-server-sqlite", "--db-path", "./db.sqlite"],
                "env": [{"name": "SQLITE_TIMEOUT", "value": "5000"}]
            },
            {
                "name": "remote",
                "type": "http",
                "url": "https://mcp.example.com",
                "headers": [{"name": "Authorization", "value": "Bearer token"}]
            }
        ]);
        let servers = parse_mcp_servers(&json).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "sqlite");
        assert_eq!(
            servers[0].transport,
            McpTransport::Stdio {
                command: "uvx".into(),
                args: vec![
                    "mcp-server-sqlite".into(),
                    "--db-path".into(),
                    "./db.sqlite".into()
                ],
                env: BTreeMap::from([("SQLITE_TIMEOUT".into(), "5000".into())]),
            }
        );
        assert_eq!(servers[1].name, "remote");
        assert_eq!(
            servers[1].transport,
            McpTransport::Http {
                url: "https://mcp.example.com".into(),
                headers: BTreeMap::from([("Authorization".into(), "Bearer token".into())]),
            }
        );

        // Verify conversion to ACP
        let acp_val = servers[0].to_acp_value();
        assert_eq!(acp_val["name"], "sqlite");
        assert_eq!(acp_val["command"], "uvx");
        assert_eq!(acp_val["env"][0]["name"], "SQLITE_TIMEOUT");

        // Verify conversion to Codex
        let (codex_name, codex_val) = servers[0].to_codex_entry();
        assert_eq!(codex_name, "sqlite");
        assert_eq!(codex_val["command"], "uvx");
        assert_eq!(codex_val["env"]["SQLITE_TIMEOUT"], "5000");

        let (codex_http_name, codex_http_val) = servers[1].to_codex_entry();
        assert_eq!(codex_http_name, "remote");
        assert_eq!(codex_http_val["url"], "https://mcp.example.com");
        assert_eq!(
            codex_http_val["http_headers"]["Authorization"],
            "Bearer token"
        );
    }

    #[test]
    fn parses_codex_dictionary_format() {
        let json = json!({
            "my server": {
                "command": "node",
                "args": ["server.js"],
                "env": {"NODE_ENV": "production"}
            }
        });
        let servers = parse_mcp_servers(&json).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "my server");

        // Sanitization on codex conversion
        let (name, val) = servers[0].to_codex_entry();
        assert_eq!(name, "my_server");
        assert_eq!(val["command"], "node");
        assert_eq!(val["env"]["NODE_ENV"], "production");
    }

    #[test]
    fn builder_methods_work_correctly() {
        let server = McpServer::stdio("github", "npx")
            .with_args(["-y", "@modelcontextprotocol/server-github"])
            .with_env("GITHUB_TOKEN", "secret");

        let (name, val) = server.to_codex_entry();
        assert_eq!(name, "github");
        assert_eq!(val["command"], "npx");
        assert_eq!(
            val["args"],
            json!(["-y", "@modelcontextprotocol/server-github"])
        );
        assert_eq!(val["env"]["GITHUB_TOKEN"], "secret");
    }
}
