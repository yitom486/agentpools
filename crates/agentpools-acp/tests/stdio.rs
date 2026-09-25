#![cfg(feature = "test-support")]

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agentpools::{AgentPool, PoolConfig, ShutdownMode, TaskError};
use agentpools_acp::{
    ACP_POOL_API_VERSION, AcpAgentOptions, AcpBackend, AcpConfig, AcpError, AcpPoolOptions,
    AcpPrompt, AcpTimeoutOptions, HostRequestHandler, SharedAcpBackend,
};
use serde_json::{Value, json};

static NEXT_LOG: AtomicU64 = AtomicU64::new(1);

fn test_config(scenario: &str) -> (AcpConfig, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "agentpools-acp-{}-{}-{}.jsonl",
        std::process::id(),
        scenario,
        NEXT_LOG.fetch_add(1, Ordering::Relaxed)
    ));
    let mut config = AcpConfig::new(
        env!("CARGO_BIN_EXE_agentpools-acp-mock-agent"),
        std::env::current_dir().unwrap(),
    );
    config
        .env
        .insert("AGENTPOOLS_MOCK_LOG".into(), path.as_os_str().into());
    config
        .env
        .insert("AGENTPOOLS_MOCK_SCENARIO".into(), scenario.into());
    config.handshake_timeout = Duration::from_secs(2);
    config.prompt_timeout = Duration::from_secs(2);
    config.close_timeout = Duration::from_secs(2);
    (config, path)
}

fn pool(config: AcpConfig) -> AgentPool<AcpBackend> {
    AgentPool::new(
        AcpBackend,
        config,
        PoolConfig {
            workers: 1,
            max_queued: 4,
        },
    )
    .unwrap()
}

fn log(path: &PathBuf) -> Vec<Value> {
    let content = fs::read_to_string(path).unwrap();
    // Polling tests may read while the mock process is still appending a line.
    // Parse only complete newline-terminated JSON-RPC log entries.
    let complete = if content.ends_with('\n') {
        content.as_str()
    } else {
        content.rsplit_once('\n').map_or("", |(head, _)| head)
    };
    complete
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn count_method(messages: &[Value], method: &str) -> usize {
    messages
        .iter()
        .filter(|message| message.get("method").and_then(Value::as_str) == Some(method))
        .count()
}

#[test]
fn real_stdio_roundtrip_reuses_session_and_forwards_mcp_config() {
    let (mut config, path) = test_config("");
    let mcp = vec![
        json!({"name":"caller-files", "command":"files-mcp", "args":["--readonly"], "env":[]}),
        json!({"name":"caller-search", "url":"http://localhost:3000/mcp", "type":"http"}),
    ];
    config.mcp_servers = mcp.clone();
    let mut pool = pool(config);
    let first = pool.submit(AcpPrompt::text("one")).unwrap().wait().unwrap();
    let second = pool.submit(AcpPrompt::text("two")).unwrap().wait().unwrap();
    assert_eq!(first.text, "mock:one");
    assert_eq!(second.text, "mock:two");
    assert_eq!(first.stop_reason, "end_turn");
    let report = pool.shutdown(ShutdownMode::Drain);
    assert_eq!(report.panicked_workers, 0);
    assert!(report.close_errors.is_empty());
    let messages = log(&path);
    assert_eq!(count_method(&messages, "initialize"), 1);
    assert_eq!(count_method(&messages, "session/new"), 1);
    assert_eq!(count_method(&messages, "session/prompt"), 2);
    assert_eq!(count_method(&messages, "session/close"), 1);
    let new = messages
        .iter()
        .find(|message| message["method"] == "session/new")
        .unwrap();
    assert_eq!(new["params"]["mcpServers"], json!(mcp));
    let _ = fs::remove_file(path);
}

#[test]
fn shared_process_routes_concurrent_sessions_and_uses_global_request_ids() {
    let (mut first, path) = test_config("shared-concurrency");
    first.mcp_servers = vec![json!({"name":"first-tool"})];
    let mut second = AcpConfig::new(
        env!("CARGO_BIN_EXE_agentpools-acp-mock-agent"),
        std::env::current_dir().unwrap(),
    );
    second.env = first.env.clone();
    second.handshake_timeout = first.handshake_timeout;
    second.prompt_timeout = first.prompt_timeout;
    second.close_timeout = first.close_timeout;
    second.mcp_servers = vec![json!({"name":"second-tool"})];

    let backend = SharedAcpBackend::new();
    let mut pool =
        AgentPool::with_agents(vec![(backend.clone(), first), (backend, second)], 4).unwrap();
    let slow = pool.submit_to(0, AcpPrompt::text("slow")).unwrap();
    let fast = pool.submit_to(1, AcpPrompt::text("fast")).unwrap();

    let fast = fast.wait().unwrap();
    let slow = slow.wait().unwrap();
    assert!(
        fast.text.ends_with(":fast"),
        "unexpected response: {fast:?}"
    );
    assert!(
        slow.text.ends_with(":slow"),
        "unexpected response: {slow:?}"
    );

    let report = pool.shutdown(ShutdownMode::Drain);
    assert_eq!(report.panicked_workers, 0);
    assert!(report.close_errors.is_empty());

    let messages = log(&path);
    assert_eq!(count_method(&messages, "initialize"), 1);
    assert_eq!(count_method(&messages, "session/new"), 2);
    assert_eq!(count_method(&messages, "session/prompt"), 2);
    assert_eq!(count_method(&messages, "session/close"), 2);

    let session_ids = messages
        .iter()
        .filter_map(|message| message.get("mockSessionCreated").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    assert_eq!(session_ids.len(), 2);
    let tool_names = messages
        .iter()
        .filter(|message| message["method"] == "session/new")
        .filter_map(|message| message["params"]["mcpServers"][0]["name"].as_str())
        .collect::<HashSet<_>>();
    assert_eq!(tool_names, HashSet::from(["first-tool", "second-tool"]));

    let request_ids = messages
        .iter()
        .filter(|message| message.get("method").is_some())
        .filter_map(|message| message.get("id").and_then(Value::as_u64))
        .collect::<Vec<_>>();
    assert_eq!(
        request_ids.iter().copied().collect::<HashSet<_>>().len(),
        request_ids.len()
    );
    let _ = fs::remove_file(path);
}

#[test]
fn language_config_builds_pool_and_forwards_mcp_servers_to_acp() {
    let path = std::env::temp_dir().join(format!(
        "agentpools-acp-language-api-{}-{}.jsonl",
        std::process::id(),
        NEXT_LOG.fetch_add(1, Ordering::Relaxed)
    ));
    let mcp = vec![json!({
        "name": "caller-calculator",
        "command": "calculator-mcp",
        "args": ["--readonly"],
        "env": [{"name": "MODE", "value": "test"}]
    })];
    let options = AcpPoolOptions {
        api_version: ACP_POOL_API_VERSION,
        agents: vec![AcpAgentOptions {
            program: env!("CARGO_BIN_EXE_agentpools-acp-mock-agent").into(),
            args: Vec::new(),
            env: BTreeMap::from([(
                "AGENTPOOLS_MOCK_LOG".into(),
                path.to_string_lossy().into_owned(),
            )]),
            cwd: std::env::current_dir().unwrap(),
            mcp_servers: mcp.clone(),
            auth_method: None,
            model: None,
            timeouts: AcpTimeoutOptions {
                handshake_ms: 2_000,
                prompt_ms: 2_000,
                close_ms: 2_000,
            },
            inherit_stderr: false,
        }],
        max_queued: 4,
    };
    let mut pool = options.build().unwrap();
    let response = pool
        .submit(AcpPrompt::text("hello from the language API"))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(response.text, "mock:hello from the language API");
    pool.shutdown(ShutdownMode::Drain);

    let messages = log(&path);
    let session_new = messages
        .iter()
        .find(|message| message["method"] == "session/new")
        .unwrap();
    assert_eq!(session_new["params"]["mcpServers"], json!(mcp));
    let _ = fs::remove_file(path);
}

struct PermissionHandler;

impl HostRequestHandler for PermissionHandler {
    fn handle(&self, method: &str, _: &Value) -> Result<Value, AcpError> {
        assert_eq!(method, "session/request_permission");
        Ok(json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}))
    }
}

#[test]
fn agent_permission_request_reaches_caller_handler() {
    let (mut config, path) = test_config("permission");
    config.host_handler = Some(Arc::new(PermissionHandler));
    let mut pool = pool(config);
    let response = pool
        .submit(AcpPrompt::text("use tool"))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(response.text, "mock:use tool");
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    let reply = messages
        .iter()
        .find(|message| message["id"] == 900)
        .unwrap();
    assert_eq!(reply["result"]["outcome"]["optionId"], "allow-once");
    let _ = fs::remove_file(path);
}

#[test]
fn permission_is_cancelled_when_caller_did_not_supply_handler() {
    let (config, path) = test_config("permission");
    let mut pool = pool(config);
    let response = pool
        .submit(AcpPrompt::text("use tool"))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(response.text, "mock:use tool");
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    let reply = messages
        .iter()
        .find(|message| message["id"] == 900)
        .unwrap();
    assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
    let _ = fs::remove_file(path);
}

#[test]
fn explicit_authentication_happens_before_session_creation() {
    let (mut config, path) = test_config("auth-required");
    config.auth_method = Some("api-key".into());
    let mut pool = pool(config);
    assert_eq!(
        pool.submit(AcpPrompt::text("hello"))
            .unwrap()
            .wait()
            .unwrap()
            .text,
        "mock:hello"
    );
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    let methods = messages
        .iter()
        .filter_map(|message| message.get("method").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        &methods[..3],
        &["initialize", "authenticate", "session/new"]
    );
    let _ = fs::remove_file(path);
}

#[test]
fn requested_model_is_set_and_verified_before_prompt() {
    let (mut config, path) = test_config("");
    config.model = Some("gpt-6-luna".into());
    let mut pool = pool(config);
    assert_eq!(
        pool.submit(AcpPrompt::text("hello"))
            .unwrap()
            .wait()
            .unwrap()
            .text,
        "mock:hello"
    );
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    let methods = messages
        .iter()
        .filter_map(|message| message.get("method").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        &methods[..4],
        &[
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/prompt"
        ]
    );
    let selected = messages
        .iter()
        .find(|message| message["method"] == "session/set_config_option")
        .unwrap();
    assert_eq!(selected["params"]["configId"], "model");
    assert_eq!(selected["params"]["value"], "gpt-6-luna");
    let _ = fs::remove_file(path);
}

#[test]
fn model_mismatch_rejects_session_before_prompt() {
    let (mut config, path) = test_config("model-ignored");
    config.model = Some("gpt-6-luna".into());
    let mut pool = pool(config);
    assert!(matches!(
        pool.submit(AcpPrompt::text("hello")).unwrap().wait(),
        Err(TaskError::Open(AcpError::Protocol(_)))
    ));
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    assert_eq!(count_method(&messages, "session/set_config_option"), 1);
    assert_eq!(count_method(&messages, "session/prompt"), 0);
    let _ = fs::remove_file(path);
}

#[test]
fn close_method_is_not_sent_when_agent_did_not_advertise_it() {
    let (config, path) = test_config("no-close");
    let mut pool = pool(config);
    pool.submit(AcpPrompt::text("hello"))
        .unwrap()
        .wait()
        .unwrap();
    pool.shutdown(ShutdownMode::Drain);
    assert_eq!(count_method(&log(&path), "session/close"), 0);
    let _ = fs::remove_file(path);
}

#[test]
fn cancellation_sends_acp_cancel_and_closes_session() {
    let (config, path) = test_config("cancel");
    let mut pool = pool(config);
    let handle = pool.submit(AcpPrompt::text("wait")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if path.exists() && count_method(&log(&path), "session/prompt") == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(count_method(&log(&path), "session/prompt"), 1);
    handle.cancel();
    assert!(matches!(
        handle.wait(),
        Err(TaskError::Run {
            source: AcpError::Cancelled,
            ..
        })
    ));
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    assert_eq!(count_method(&messages, "session/cancel"), 1);
    assert_eq!(count_method(&messages, "session/close"), 1);
    let _ = fs::remove_file(path);
}

#[test]
fn incompatible_protocol_version_fails_before_session_creation() {
    let (config, path) = test_config("bad-version");
    let mut pool = pool(config);
    assert!(matches!(
        pool.submit(AcpPrompt::text("hello")).unwrap().wait(),
        Err(TaskError::Open(AcpError::Protocol(_)))
    ));
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    assert_eq!(count_method(&messages, "initialize"), 1);
    assert_eq!(count_method(&messages, "session/new"), 0);
    let _ = fs::remove_file(path);
}

#[test]
fn agent_error_discards_session_before_next_task() {
    let (config, path) = test_config("prompt-error");
    let mut pool = pool(config);
    for _ in 0..2 {
        assert!(matches!(
            pool.submit(AcpPrompt::text("fail")).unwrap().wait(),
            Err(TaskError::Run {
                source: AcpError::Remote { .. },
                ..
            })
        ));
    }
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    assert_eq!(count_method(&messages, "initialize"), 2);
    assert_eq!(count_method(&messages, "session/new"), 2);
    assert_eq!(count_method(&messages, "session/close"), 2);
    let _ = fs::remove_file(path);
}

#[test]
fn remote_error_feedback_retries_in_the_same_acp_session() {
    let (config, path) = test_config("recover-on-feedback");
    let mut pool = pool(config);
    let response = pool
        .submit_retrying(
            AcpPrompt::text("initial"),
            NonZeroUsize::new(2).unwrap(),
            |error, attempt| {
                assert_eq!(attempt, 1);
                assert!(matches!(error, AcpError::Remote { .. }));
                Some(AcpPrompt::text(format!(
                    "The previous attempt failed: {error}. Please retry."
                )))
            },
        )
        .unwrap()
        .wait()
        .unwrap();
    assert!(response.text.contains("mock failure"));
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    assert_eq!(count_method(&messages, "initialize"), 1);
    assert_eq!(count_method(&messages, "session/new"), 1);
    assert_eq!(count_method(&messages, "session/prompt"), 2);
    assert_eq!(count_method(&messages, "session/close"), 1);
    let _ = fs::remove_file(path);
}

#[cfg(feature = "mcp-example")]
#[test]
fn injected_official_mcp_add_tool_is_called_through_mock_acp_agent() {
    let (mut config, path) = test_config("mcp-add");
    config.mcp_servers = vec![json!({
        "name": "calculator",
        "command": env!("CARGO_BIN_EXE_agentpools-mcp-add"),
        "args": [],
        "env": []
    })];
    let mut pool = pool(config);
    let response = pool
        .submit(AcpPrompt::text("add 2 and 3"))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(response.text, "mock:5");
    let response = pool
        .submit(AcpPrompt::text("add 8 and 13"))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(response.text, "mock:21");
    pool.shutdown(ShutdownMode::Drain);
    let messages = log(&path);
    assert_eq!(count_method(&messages, "session/new"), 1);
    assert_eq!(count_method(&messages, "session/prompt"), 2);
    let _ = fs::remove_file(path);
}
