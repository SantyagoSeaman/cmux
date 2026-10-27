//! p01-process-240 (acpmux-audit finding 10): a per-chat MCP server (`acpmux
//! forum-mcp` and the other `*-mcp` words, `agent_tools.rs`) runs on one
//! runtime thread, not a worker per core: before, each was a multi-thread
//! runtime with 12 to 16 idle threads, four of them per chat.
#![cfg(target_os = "linux")]

#[path = "support/daemon_env.rs"]
mod daemon_env;
// No agent starts here: the server only answers `initialize`.
const HERMETIC: daemon_env::Process = daemon_env::Process::deny();

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn a_chats_mcp_server_runs_on_one_runtime_thread() {
    let scratch = std::env::temp_dir().join(format!("acpmux-mcp-threads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let mut server = Command::new(daemon_env::bin())
        .arg("forum-mcp")
        .env_clear()
        .envs(daemon_env::vars(&scratch, daemon_env::Agents::Deny, &[]))
        .env("ACPMUX_SESSION_ID", "s-threads")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start forum-mcp");
    let mut stdin = server.stdin.take().unwrap();
    let mut stdout = BufReader::new(server.stdout.take().unwrap());
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    writeln!(stdin, "{hello}").unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["result"]["serverInfo"]["name"], "cmux-forum", "{reply}");
    let names: Vec<String> = std::fs::read_dir(format!("/proc/{}/task", server.id()))
        .expect("the server's threads")
        .filter_map(Result::ok)
        .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
        .map(|name| name.trim().to_owned())
        .collect();
    // The runtime thread and the blocking workers that read stdin and write
    // stdout (`MCP_SERVER_BLOCKING_THREADS`), with room for the hang watch.
    assert!(names.len() <= 6, "{} threads: {names:?}", names.len());
    drop(stdin);
    assert!(server.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&scratch);
}
