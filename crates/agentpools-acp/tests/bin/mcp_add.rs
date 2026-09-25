//! A small stdio MCP server for exercising ACP MCP passthrough.
//! Built with the official Model Context Protocol Rust SDK (`rmcp`).

use rmcp::{
    ServiceExt, handler::server::wrapper::Parameters, schemars, tool, tool_router, transport::stdio,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AddParams {
    a: i64,
    b: i64,
}

#[derive(Clone)]
struct Calculator {
    log_path: Option<PathBuf>,
}

#[tool_router(server_handler)]
impl Calculator {
    #[tool(description = "Add two integers")]
    fn add(&self, Parameters(AddParams { a, b }): Parameters<AddParams>) -> String {
        let answer = match a.checked_add(b) {
            Some(sum) => sum.to_string(),
            None => "integer overflow".to_string(),
        };
        if let Some(path) = &self.log_path {
            let event = serde_json::json!({
                "pid": std::process::id(), "a": a, "b": b, "answer": answer
            });
            if let Err(error) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| writeln!(file, "{event}"))
            {
                eprintln!("cannot log MCP add call: {error}");
            }
        }
        answer
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = Calculator {
        log_path: std::env::var_os("AGENTPOOLS_MCP_LOG").map(PathBuf::from),
    }
    .serve(stdio())
    .await?;
    service.waiting().await?;
    Ok(())
}
