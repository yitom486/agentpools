#![cfg(feature = "mcp-example")]

use std::time::Duration;

use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::{Value, json};
use tokio::process::Command;

#[tokio::test]
async fn official_mcp_sdk_add_server_lists_and_calls_tool() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let client = ()
            .serve(TokioChildProcess::new(Command::new(env!(
                "CARGO_BIN_EXE_agentpools-mcp-add"
            )))?)
            .await?;
        let tools = client.list_all_tools().await?;
        assert!(tools.iter().any(|tool| tool.name == "add"));
        let arguments = json!({"a": 2, "b": 3})
            .as_object()
            .expect("literal object")
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("add").with_arguments(arguments))
            .await?;
        let result: Value = serde_json::to_value(result)?;
        assert_eq!(result["content"][0]["text"], "5");
        client.cancel().await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
    .await
    .expect("MCP round trip timed out")
    .unwrap();
}
