use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = std::env::var("AGENTPOOLS_MOCK_SCENARIO").unwrap_or_default();
    let log = std::env::var("AGENTPOOLS_MOCK_LOG")?;
    let mut input = BufReader::new(io::stdin().lock());
    // Keep the stdout handle, not a lock held for the lifetime of this loop:
    // the shared-concurrency scenario writes replies from background threads.
    let mut output = io::BufWriter::new(io::stdout());
    let mut line = String::new();
    let mut authenticated = false;
    let mut selected_model = "mock-default".to_string();
    let mut next_session_id = 1_u64;
    #[cfg(feature = "mcp-example")]
    let mut mcp_servers = Vec::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line)?;
        record(Path::new(&log), &message)?;
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        match message.get("method").and_then(Value::as_str).unwrap_or("") {
            "initialize" => {
                respond(
                    &mut output,
                    id,
                    json!({
                        "protocolVersion": if scenario == "bad-version" { 999 } else { 1 },
                        "agentInfo": {"name": "agentpools-mock"},
                        "agentCapabilities": if scenario == "no-close" {
                            json!({"sessionCapabilities": {}})
                        } else {
                            json!({"sessionCapabilities": {"close": {}}})
                        }
                    }),
                )?;
            }
            "authenticate" => {
                authenticated = true;
                respond(&mut output, id, json!({}))?;
            }
            "session/new" if scenario == "auth-required" && !authenticated => {
                send(
                    &mut output,
                    json!({
                        "jsonrpc":"2.0", "id":id,
                        "error":{"code":-32000,"message":"Authentication required"}
                    }),
                )?;
            }
            "session/new" => {
                #[cfg(feature = "mcp-example")]
                {
                    mcp_servers = message["params"]["mcpServers"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                }
                let session_id = if scenario == "shared-concurrency" {
                    let session_id = format!("mock-session-{next_session_id}");
                    next_session_id += 1;
                    record(Path::new(&log), &json!({"mockSessionCreated": session_id}))?;
                    session_id
                } else {
                    "mock-session".to_string()
                };
                respond(
                    &mut output,
                    id,
                    json!({
                        "sessionId":session_id,
                        "configOptions":[{"id":"model","currentValue":selected_model}]
                    }),
                )?;
            }
            "session/set_config_option" => {
                if message["params"]["configId"] != "model" {
                    send(
                        &mut output,
                        json!({
                            "jsonrpc":"2.0", "id":id,
                            "error":{"code":-32602,"message":"unsupported config option"}
                        }),
                    )?;
                    continue;
                }
                selected_model = message["params"]["value"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if scenario == "model-ignored" {
                    selected_model = "mock-default".to_string();
                }
                respond(
                    &mut output,
                    id,
                    json!({
                        "configOptions":[{"id":"model","currentValue":selected_model}]
                    }),
                )?;
            }
            "session/prompt" => {
                if scenario == "shared-concurrency" {
                    let session_id = message["params"]["sessionId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let text = message
                        .pointer("/params/prompt/0/text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let delay = if text == "slow" { 100 } else { 5 };
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(delay));
                        let answer = format!("{session_id}:{text}");
                        for fragment in ["mock:", answer.as_str()] {
                            let _ = send_stdout(json!({
                                "jsonrpc":"2.0", "method":"session/update",
                                "params": {"sessionId":session_id, "update":{
                                    "sessionUpdate":"agent_message_chunk",
                                    "content":{"type":"text","text":fragment}
                                }}
                            }));
                        }
                        let _ = send_stdout(json!({
                            "jsonrpc":"2.0", "id":id,
                            "result":{"stopReason":"end_turn"}
                        }));
                    });
                    continue;
                }
                if let Ok(delay_ms) = std::env::var("AGENTPOOLS_MOCK_DELAY_MS")
                    && let Ok(delay_ms) = delay_ms.parse::<u64>()
                {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                if scenario == "permission" {
                    send(
                        &mut output,
                        json!({
                            "jsonrpc":"2.0", "id":900,
                            "method":"session/request_permission",
                            "params": {"sessionId":"mock-session", "options":[
                                {"optionId":"allow-once","name":"Allow","kind":"allow_once"},
                                {"optionId":"reject-once","name":"Reject","kind":"reject_once"}
                            ]}
                        }),
                    )?;
                    let mut reply = String::new();
                    input.read_line(&mut reply)?;
                    record(Path::new(&log), &serde_json::from_str::<Value>(&reply)?)?;
                }
                if scenario == "cancel" {
                    let mut next = String::new();
                    input.read_line(&mut next)?;
                    record(Path::new(&log), &serde_json::from_str::<Value>(&next)?)?;
                }
                let text = message
                    .pointer("/params/prompt/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if scenario == "prompt-error"
                    || (scenario == "recover-on-feedback" && text == "initial")
                {
                    send(
                        &mut output,
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"mock failure"}}),
                    )?;
                    continue;
                }
                #[cfg(feature = "mcp-example")]
                let tool_result = if scenario == "mcp-add" {
                    call_mcp_add(&mcp_servers, text)?
                } else {
                    text.to_string()
                };
                #[cfg(not(feature = "mcp-example"))]
                let tool_result = text.to_string();
                for fragment in ["mock:", tool_result.as_str()] {
                    send(
                        &mut output,
                        json!({
                            "jsonrpc":"2.0", "method":"session/update",
                            "params": {"sessionId":"mock-session", "update":{
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":fragment}
                            }}
                        }),
                    )?;
                }
                respond(&mut output, id, json!({"stopReason":"end_turn"}))?;
            }
            "session/delete" => {
                respond(&mut output, id, json!({}))?;
            }
            "session/close" => {
                respond(&mut output, id, json!({}))?;
                if scenario != "shared-concurrency" {
                    break;
                }
            }
            _ => respond(&mut output, id, json!({}))?,
        }
    }
    Ok(())
}

#[cfg(feature = "mcp-example")]
fn call_mcp_add(servers: &[Value], prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
    use tokio::process::Command;

    let words = prompt.split_whitespace().collect::<Vec<_>>();
    if words.len() != 4 || words[0] != "add" || words[2] != "and" {
        return Err(io::Error::other("expected 'add <a> and <b>'").into());
    }
    let a: i64 = words[1].parse()?;
    let b: i64 = words[3].parse()?;

    let server = servers
        .iter()
        .find(|server| server["name"] == "calculator")
        .ok_or_else(|| io::Error::other("calculator MCP server was not injected"))?;
    let program = server["command"]
        .as_str()
        .ok_or_else(|| io::Error::other("calculator command is missing"))?;
    let mut command = Command::new(program);
    if let Some(args) = server["args"].as_array() {
        for arg in args {
            command.arg(
                arg.as_str()
                    .ok_or_else(|| io::Error::other("invalid MCP arg"))?,
            );
        }
    }
    if let Some(env) = server["env"].as_array() {
        for entry in env {
            let name = entry["name"]
                .as_str()
                .ok_or_else(|| io::Error::other("invalid MCP environment name"))?;
            let value = entry["value"]
                .as_str()
                .ok_or_else(|| io::Error::other("invalid MCP environment value"))?;
            command.env(name, value);
        }
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let client = ().serve(TokioChildProcess::new(command)?).await?;
        let arguments = json!({"a": a, "b": b})
            .as_object()
            .expect("literal object")
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("add").with_arguments(arguments))
            .await?;
        let result = serde_json::to_value(result)?;
        let sum = result["content"][0]["text"]
            .as_str()
            .ok_or_else(|| io::Error::other("MCP add returned no text"))?
            .to_string();
        client.cancel().await?;
        Ok(sum)
    })
}

fn record(path: &Path, message: &Value) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{message}")
}

fn respond(out: &mut impl Write, id: Value, result: Value) -> io::Result<()> {
    send(out, json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn send(out: &mut impl Write, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn send_stdout(value: Value) -> io::Result<()> {
    static OUTPUT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = OUTPUT_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    send(&mut output, value)
}
