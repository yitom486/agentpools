use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn emit(value: Value) {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn main() {
    let mode = std::env::args().nth(1).expect("mock mode");
    let mut count = 0;
    let mut thread_count = 0;
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if mode == "codex" {
            match message["method"].as_str() {
                Some("initialize") => emit(json!({"id":message["id"],"result":{}})),
                Some("thread/start") => {
                    thread_count += 1;
                    let mcp_count = message
                        .pointer("/params/config/mcp_servers")
                        .and_then(Value::as_object)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    let thread_id = if mcp_count > 0 {
                        format!("thread-mcp-{mcp_count}")
                    } else {
                        format!("thread-{thread_count}")
                    };
                    emit(json!({"id":message["id"],"result":{"thread":{"id":thread_id}}}));
                }
                Some("turn/start") => {
                    count += 1;
                    let turn_id = format!("turn-{count}");
                    let text = message
                        .pointer("/params/input/0/text")
                        .and_then(Value::as_str)
                        .unwrap();
                    let thread_id = message
                        .pointer("/params/threadId")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let answer = if thread_id.starts_with("thread-mcp-") {
                        let suffix = thread_id.strip_prefix("thread-").unwrap();
                        format!("codex:{text}:{count}:{suffix}")
                    } else {
                        format!("codex:{text}:{count}")
                    };
                    emit(
                        json!({"method":"item/completed","params":{"threadId":thread_id,"turnId":turn_id,"item":{"type":"agentMessage","phase":"final_answer","text":answer}}}),
                    );
                    emit(json!({"id":message["id"],"result":{"turn":{"id":turn_id}}}));
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":thread_id,"turn":{"id":turn_id,"status":"completed"}}}),
                    );
                }
                Some("turn/interrupt") => {
                    emit(json!({"id":message["id"],"result":{}}));
                }
                Some("thread/delete") => {
                    emit(json!({"id":message["id"],"result":{}}));
                }
                _ => {}
            }
        } else if message["type"] == "get_state" {
            emit(
                json!({"id":message["id"],"type":"response","command":"get_state","success":true,"data":{"sessionId":"mock"}}),
            );
        } else if message["type"] == "prompt" {
            count += 1;
            let text = message["message"].as_str().unwrap();
            emit(
                json!({"id":message["id"],"type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}),
            );
            emit(
                json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":format!("pi:{text}:{count}")}],"stopReason":"end"}}),
            );
            emit(json!({"type":"agent_settled"}));
        } else if message["type"] == "abort" {
            emit(json!({"type":"agent_settled"}));
        }
    }
}
