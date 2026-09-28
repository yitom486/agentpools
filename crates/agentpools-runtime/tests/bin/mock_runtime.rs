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
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if mode == "codex" {
            match message["method"].as_str() {
                Some("initialize") => emit(json!({"id":message["id"],"result":{}})),
                Some("thread/start") => {
                    emit(json!({"id":message["id"],"result":{"thread":{"id":"thread-1"}}}))
                }
                Some("turn/start") => {
                    count += 1;
                    let turn_id = format!("turn-{count}");
                    let text = message
                        .pointer("/params/input/0/text")
                        .and_then(Value::as_str)
                        .unwrap();
                    emit(
                        json!({"method":"item/completed","params":{"turnId":turn_id,"item":{"type":"agentMessage","phase":"final_answer","text":format!("codex:{text}:{count}")}}}),
                    );
                    emit(json!({"id":message["id"],"result":{"turn":{"id":turn_id}}}));
                    emit(
                        json!({"method":"turn/completed","params":{"turn":{"id":turn_id,"status":"completed"}}}),
                    );
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
        }
    }
}
