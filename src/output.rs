//! Human/JSON output helpers shared across CLI handlers.

use serde::Serialize;
use serde_json::json;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
}

pub fn print_value<T: Serialize>(mode: OutputMode, value: &T, human: impl FnOnce(&T) -> String) {
    match mode {
        OutputMode::Json => println!("{}", serde_json::to_string(value).unwrap()),
        OutputMode::Human => println!("{}", human(value)),
    }
}

pub fn print_error(mode: OutputMode, msg: &str, code: i32) {
    match mode {
        OutputMode::Json => {
            let v = json!({ "error": msg, "code": code });
            eprintln!("{}", serde_json::to_string(&v).unwrap());
        }
        OutputMode::Human => {
            eprintln!("servant: {msg}");
        }
    }
}
