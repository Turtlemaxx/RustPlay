//! Linux stand-in for the Bluetooth mouse/keyboard remote (`remote.rs` uses
//! the Windows Bluetooth LE peripheral APIs). Turning it on reports an error.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::airplay::hub::Hub;

pub struct Remote {
    state: Mutex<Value>,
}

impl Remote {
    pub fn start(_hub: Arc<Hub>) -> Arc<Remote> {
        Arc::new(Remote { state: Mutex::new(json!({ "type": "remote", "state": "off" })) })
    }

    pub fn shutdown(&self) {}

    pub fn command(&self, request: &Value) -> Value {
        match request["type"].as_str().unwrap_or_default() {
            "remote-status" => self.state.lock().unwrap().clone(),
            "remote-enable" => {
                let on = request["on"].as_bool().unwrap_or(false);
                let state = if on {
                    json!({ "type": "remote", "state": "error",
                            "detail": "Mouse and keyboard control is only available on Windows." })
                } else {
                    json!({ "type": "remote", "state": "off" })
                };
                *self.state.lock().unwrap() = state.clone();
                state
            }
            "remote-mouse" | "remote-keys" => json!({ "ok": true }),
            _ => json!({ "ok": false, "error": "unknown command" }),
        }
    }
}
