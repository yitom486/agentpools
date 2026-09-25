//! Set CODEX_ACP_ENTRY to the installed package's dist/index.js, then run:
//! cargo run -p agentpools-acp --example codex

use std::path::PathBuf;
use std::time::Duration;

use agentpools::{AgentPool, PoolConfig, ShutdownMode};
use agentpools_acp::{AcpBackend, AcpConfig, AcpPrompt};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let entry = std::env::var_os("CODEX_ACP_ENTRY")
        .ok_or("set CODEX_ACP_ENTRY to codex-acp/dist/index.js")?;
    let mut config = AcpConfig::new("node", std::env::current_dir()?);
    config.args.push(entry);
    config.model = Some("gpt-6-luna".into());
    config.handshake_timeout = Duration::from_secs(60);
    config.prompt_timeout = Duration::from_secs(120);
    config.inherit_stderr = true;
    let prompt = if let Some(add_bin) = std::env::var_os("AGENTPOOLS_MCP_ADD_BIN") {
        let add_bin = PathBuf::from(add_bin);
        if !add_bin.is_absolute() {
            return Err("AGENTPOOLS_MCP_ADD_BIN must be an absolute path".into());
        }
        config.mcp_servers.push(serde_json::json!({
            "name": "calculator",
            "command": add_bin.to_string_lossy(),
            "args": [],
            "env": []
        }));
        "Use the calculator add tool to add 2 and 3. Reply with only the result."
    } else {
        "Reply with exactly OK."
    };
    let mut pool = AgentPool::new(
        AcpBackend,
        config,
        PoolConfig {
            workers: 1,
            max_queued: 1,
        },
    )?;
    let response = pool
        .submit(AcpPrompt::text(prompt))
        .map_err(|_| "could not submit prompt")?
        .wait()?;
    println!("{}", response.text);
    let report = pool.shutdown(ShutdownMode::Drain);
    if report.panicked_workers != 0 || !report.close_errors.is_empty() {
        return Err("ACP agent did not close cleanly".into());
    }
    Ok(())
}
