//! p01-process-240 (acpmux-audit finding 10): a per-chat stdio tool server
//! (`acpmux forum-mcp` and the other `*-mcp` words, `secret_mcp::serve`)
//! keeps one daemon connection for all its calls, overlapping ones too, and
//! connects again only after the daemon closed it. Before, every call made a
//! new connection. A stand-in daemon here counts the connections it accepts.
#![cfg(unix)]

#[path = "support/daemon_env.rs"]
mod daemon_env;
// No agent starts here: the stand-in daemon answers the tool calls itself.
const HERMETIC: daemon_env::Process = daemon_env::Process::deny();

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(20);

/// A request the stand-in daemon got on its `conn`-th connection (from 0);
/// `stream` answers it, or closes that connection.
struct Request {
    conn: usize,
    id: Value,
    method: String,
    stream: UnixStream,
}

impl Request {
    fn reply(&mut self, result: Value) {
        let line = json!({"jsonrpc": "2.0", "id": self.id, "result": result});
        writeln!(self.stream, "{line}").unwrap();
    }
}

/// Listens at `socket`: answers each connection's `initialize` and hands
/// every other request to the test, numbered by the connection it came on.
fn stand_in_daemon(socket: &std::path::Path) -> mpsc::Receiver<Request> {
    let listener = UnixListener::bind(socket).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (conn, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { return };
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { return };
                    let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                    let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str())
                    else {
                        continue;
                    };
                    if method == "initialize" {
                        let reply = json!({"jsonrpc": "2.0", "id": id, "result": {}});
                        writeln!(writer, "{reply}").unwrap();
                        continue;
                    }
                    let stream = writer.try_clone().unwrap();
                    let request = Request { conn, id: id.clone(), method: method.into(), stream };
                    if tx.send(request).is_err() {
                        return;
                    }
                }
            });
        }
    });
    rx
}

/// The tool server's answers, by id, in whatever order they come.
struct Answers {
    lines: mpsc::Receiver<Value>,
    early: HashMap<String, Value>,
}

impl Answers {
    fn of(&mut self, id: u64) -> Value {
        let key = id.to_string();
        loop {
            if let Some(answer) = self.early.remove(&key) {
                return answer;
            }
            let answer = self.lines.recv_timeout(WAIT).expect("an answer from forum-mcp");
            self.early.insert(answer["id"].to_string(), answer);
        }
    }
}

#[test]
fn a_tool_server_keeps_one_daemon_connection_across_its_calls() {
    let scratch = std::env::temp_dir().join(format!("acpmux-mcp-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let socket = scratch.join("d.sock");
    let requests = stand_in_daemon(&socket);
    let mut server = daemon_env::acpmux(&scratch)
        .arg("forum-mcp")
        .env("ACPMUX_SOCKET", &socket)
        .env("ACPMUX_SESSION_ID", "s-link")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start forum-mcp");
    let mut stdin = server.stdin.take().unwrap();
    let stdout = BufReader::new(server.stdout.take().unwrap());
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            let Ok(answer) = serde_json::from_str::<Value>(&line) else { continue };
            if tx.send(answer).is_err() {
                return;
            }
        }
    });
    let mut answers = Answers { lines, early: HashMap::new() };
    let mut send = move |id: u64, method: &str, params: Value| {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(stdin, "{message}").unwrap();
    };
    let tool = |name: &str| json!({"name": name, "arguments": {"key": "k", "body": "b"}});
    let next = || requests.recv_timeout(WAIT).expect("a request at the stand-in daemon");
    let posted = json!({"post": {"id": "p-1"}, "pushed": [], "pending": []});

    send(0, "initialize", json!({}));
    assert_eq!(answers.of(0)["result"]["serverInfo"]["name"], "cmux-forum");

    // Three calls in flight at once (none answered until all three arrived):
    // they share the one connection the first call made.
    for id in 1..=3 {
        send(id, "tools/call", tool("announce"));
    }
    let mut held: Vec<Request> = (0..3).map(|_| next()).collect();
    for request in &mut held {
        assert_eq!((request.conn, request.method.as_str()), (0, "_acpmux/forum/announce"));
        request.reply(posted.clone());
    }
    for id in 1..=3 {
        let answer = answers.of(id);
        assert_eq!(answer["result"]["isError"], false, "{answer}");
    }
    // Calls one after another: still that connection.
    for id in 4..=5 {
        send(id, "tools/call", tool("rules"));
        let mut request = next();
        assert_eq!((request.conn, request.method.as_str()), (0, "_acpmux/forum/rules"));
        request.reply(json!({"rules": []}));
        let answer = answers.of(id);
        assert_eq!(answer["result"]["isError"], false, "{answer}");
    }

    // The daemon closes the connection during a call (a restart): that call
    // fails, and the next one makes exactly one new connection and succeeds.
    send(6, "tools/call", tool("announce"));
    let lost = next();
    assert_eq!(lost.conn, 0);
    lost.stream.shutdown(std::net::Shutdown::Both).unwrap();
    let answer = answers.of(6);
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    for id in 7..=8 {
        send(id, "tools/call", tool("rules"));
        let mut request = next();
        assert_eq!((request.conn, request.method.as_str()), (1, "_acpmux/forum/rules"));
        request.reply(json!({"rules": []}));
        let answer = answers.of(id);
        assert_eq!(answer["result"]["isError"], false, "{answer}");
    }
    assert!(requests.try_recv().is_err(), "no request beyond the calls");

    drop(send);
    assert!(server.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&scratch);
}
