use agentpools::ShutdownMode;

use agentpools_runtime::{RuntimePrompt, build_pool};
use serde_json::json;

#[test]
fn mixed_native_workers_keep_their_own_sessions() {
    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();
    let options = json!({
        "apiVersion": 2,
        "maxQueued": 4,
        "agents": [
            {"runtime":"codexAppServer","program":program,"args":["codex"],"cwd":cwd},
            {"runtime":"piRpc","program":program,"args":["pi"],"cwd":cwd}
        ]
    });
    let mut pool = build_pool(&options.to_string()).unwrap();
    let mut codex = pool.acquire_to(0).unwrap();
    let mut pi = pool.acquire_to(1).unwrap();
    assert_eq!(pool.status().active, 2);
    assert_eq!(
        codex.ask(RuntimePrompt::text("hello")).unwrap().text,
        "codex:hello:1"
    );
    assert_eq!(
        codex.ask(RuntimePrompt::text("again")).unwrap().text,
        "codex:again:2"
    );
    assert_eq!(
        pi.ask(RuntimePrompt::text("hello")).unwrap().text,
        "pi:hello:1"
    );
    assert_eq!(
        pi.ask(RuntimePrompt::text("again")).unwrap().text,
        "pi:again:2"
    );
    codex.finish().unwrap();
    pi.finish().unwrap();
    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());
}

#[test]
fn v2_rejects_unknown_runtime_and_non_text_prompts() {
    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();
    let unknown =
        json!({"apiVersion":2,"agents":[{"runtime":"unknown","program":program,"cwd":cwd}]});
    assert!(build_pool(&unknown.to_string()).is_err());
    let options = json!({"apiVersion":2,"agents":[{"runtime":"piRpc","program":program,"args":["pi"],"cwd":cwd}]});
    let mut pool = build_pool(&options.to_string()).unwrap();
    let mut lease = pool.acquire().unwrap();
    let prompt = RuntimePrompt {
        content: vec![json!({"type":"image","data":"..."})],
    };
    assert!(lease.ask(prompt).is_err());
    assert_eq!(
        lease.ask(RuntimePrompt::text("valid")).unwrap().text,
        "pi:valid:1"
    );
    lease.finish().unwrap();
    let _ = pool.shutdown(ShutdownMode::Drain);
}

#[test]
fn acp_configuration_works_in_both_api_versions() {
    let cwd = std::env::current_dir().unwrap();
    for version in [1, 2] {
        let agent = if version == 1 {
            json!({"program":"lazy-acp-process","cwd":cwd})
        } else {
            json!({"runtime":"acp","program":"lazy-acp-process","cwd":cwd})
        };
        let options = json!({"apiVersion":version,"agents":[agent]});
        let mut pool = build_pool(&options.to_string()).unwrap();
        assert_eq!(pool.status().active, 0);
        let _ = pool.shutdown(ShutdownMode::CancelPending);
    }
}

#[test]
fn native_workers_handle_cancellation_and_reuse() {
    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();
    let options = json!({
        "apiVersion": 2,
        "maxQueued": 4,
        "agents": [
            {"runtime":"codexAppServer","program":program,"args":["codex"],"cwd":cwd},
            {"runtime":"piRpc","program":program,"args":["pi"],"cwd":cwd}
        ]
    });
    let mut pool = build_pool(&options.to_string()).unwrap();
    let mut codex = pool.acquire_to(0).unwrap();
    let cancellation = agentpools::CancellationToken::default();
    cancellation.cancel();
    let err = codex.ask_with_cancellation(RuntimePrompt::text("cancel me"), cancellation);
    assert!(err.is_err());
    let res = codex.ask(RuntimePrompt::text("after cancel")).unwrap();
    assert!(res.text.contains("after cancel"));
    codex.finish().unwrap();

    let mut pi = pool.acquire_to(1).unwrap();
    let cancellation = agentpools::CancellationToken::default();
    cancellation.cancel();
    let err = pi.ask_with_cancellation(RuntimePrompt::text("cancel pi"), cancellation);
    assert!(err.is_err());
    let res = pi.ask(RuntimePrompt::text("after pi cancel")).unwrap();
    assert!(res.text.contains("after pi cancel"));
    pi.finish().unwrap();

    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());
}

#[test]
fn codex_and_acp_support_unified_mcp_configurations() {
    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();

    // 1. Codex with array of MCP servers
    let options_array = json!({
        "apiVersion": 2,
        "agents": [
            {
                "runtime": "codexAppServer",
                "program": program,
                "args": ["codex"],
                "cwd": cwd,
                "mcpServers": [
                    {
                        "name": "sqlite",
                        "command": "uvx",
                        "args": ["mcp-server-sqlite"]
                    },
                    {
                        "name": "remote-api",
                        "type": "http",
                        "url": "https://mcp.example.com",
                        "headers": [{"name": "Authorization", "value": "Bearer token"}]
                    }
                ]
            }
        ]
    });
    let mut pool = build_pool(&options_array.to_string()).unwrap();
    let mut lease = pool.acquire().unwrap();
    let res = lease.ask(RuntimePrompt::text("query-db")).unwrap();
    // mock_runtime echoes mcp count from thread_id
    assert_eq!(res.text, "codex:query-db:1:mcp-2");
    lease.finish().unwrap();
    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());

    // 2. Codex with dictionary format (standard Codex style)
    let options_dict = json!({
        "apiVersion": 2,
        "agents": [
            {
                "runtime": "codexAppServer",
                "program": program,
                "args": ["codex"],
                "cwd": cwd,
                "mcpServers": {
                    "tool_one": {
                        "command": "npx",
                        "args": ["tool-one"]
                    }
                }
            }
        ]
    });
    let mut pool = build_pool(&options_dict.to_string()).unwrap();
    let mut lease = pool.acquire().unwrap();
    let res = lease.ask(RuntimePrompt::text("run-tool")).unwrap();
    assert_eq!(res.text, "codex:run-tool:1:mcp-1");
    lease.finish().unwrap();
    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());

    // 3. ACP accepting dictionary format and normalizing it
    let options_acp = json!({
        "apiVersion": 2,
        "agents": [
            {
                "runtime": "acp",
                "program": "lazy-acp-process",
                "cwd": cwd,
                "mcpServers": {
                    "sqlite": {
                        "command": "uvx",
                        "args": ["mcp-server-sqlite"]
                    }
                }
            }
        ]
    });
    let pool = build_pool(&options_acp.to_string()).unwrap();
    assert_eq!(pool.status().active, 0);

    // 4. Pi rejects mcpServers gracefully
    let options_pi = json!({
        "apiVersion": 2,
        "agents": [
            {
                "runtime": "piRpc",
                "program": program,
                "args": ["pi"],
                "cwd": cwd,
                "mcpServers": [
                    { "name": "sqlite", "command": "uvx" }
                ]
            }
        ]
    });
    let err = build_pool(&options_pi.to_string()).err().unwrap();
    assert!(err.contains("Pi RPC runtime does not currently support dynamic mcpServers"));
}

#[test]
fn rust_builder_api_configures_mcp_servers() {
    use agentpools::{PoolConfig, ShutdownMode};
    use agentpools_runtime::{McpServer, NativeConfig, RuntimeBackend, RuntimeConfig};

    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();

    let mcp = McpServer::stdio("sqlite", "uvx")
        .with_arg("mcp-server-sqlite")
        .with_env("SQLITE_PATH", "./app.db");

    let config = NativeConfig::codex(program, cwd)
        .with_arg("codex")
        .with_mcp_server(mcp);

    let mut pool = agentpools::AgentPool::new(
        RuntimeBackend,
        RuntimeConfig::Native(config),
        PoolConfig {
            workers: 1,
            max_queued: 4,
        },
    )
    .unwrap();
    let mut lease = pool.acquire().unwrap();
    let res = lease.ask(RuntimePrompt::text("check")).unwrap();
    assert_eq!(res.text, "codex:check:1:mcp-1");
    lease.finish().unwrap();
    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());
}

#[test]
fn shared_codex_app_server_multiplexes_sessions() {
    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();
    let options = json!({
        "apiVersion": 2,
        "sharedProcess": true,
        "maxQueued": 4,
        "agents": [
            {"runtime":"codexAppServer","program":program,"args":["codex"],"cwd":cwd,"ephemeral":true},
            {"runtime":"codexAppServer","program":program,"args":["codex"],"cwd":cwd,"ephemeral":true}
        ]
    });
    let mut pool = build_pool(&options.to_string()).unwrap();
    let mut w1 = pool.acquire_to(0).unwrap();
    let mut w2 = pool.acquire_to(1).unwrap();
    assert_eq!(pool.status().active, 2);

    let res1 = w1.ask(RuntimePrompt::text("task-1")).unwrap();
    assert_eq!(res1.text, "codex:task-1:1");

    let res2 = w2.ask(RuntimePrompt::text("task-2")).unwrap();
    assert_eq!(res2.text, "codex:task-2:2");

    w1.finish().unwrap();
    w2.finish().unwrap();
    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());
}

#[test]
fn codex_ephemeral_flag_is_configurable() {
    use agentpools::ShutdownMode;
    use agentpools_runtime::NativeConfig;

    let program = env!("CARGO_BIN_EXE_mock_runtime");
    let cwd = std::env::current_dir().unwrap();

    let cfg = NativeConfig::codex(program, cwd.clone()).with_ephemeral(false);
    let codex_cfg = cfg.to_codex_config();
    assert!(!codex_cfg.ephemeral);

    let options_true = json!({
        "apiVersion": 2,
        "agents": [
            {"runtime":"codexAppServer","program":program,"args":["codex"],"cwd":cwd,"ephemeral":true}
        ]
    });
    let mut pool = build_pool(&options_true.to_string()).unwrap();
    let mut lease = pool.acquire().unwrap();
    let res = lease.ask(RuntimePrompt::text("hello ephemeral")).unwrap();
    assert_eq!(res.text, "codex:hello ephemeral:1");
    lease.finish().unwrap();
    assert!(pool.shutdown(ShutdownMode::Drain).close_errors.is_empty());
}
