//! `app-children-v1` (p01-process-239): the daemon starts, reads, stops and
//! lists the app's long-lived children for the verified app only, and the
//! children outlive the app's connection under the lifeline rule.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::children::utf8_complete;
use crate::server::origin_gate::{
    set_peer_key_for_test, set_role_for_test, set_verified_app_for_test,
};
use crate::server::*;

struct Conn {
    client: u64,
    writer: MessageWriter,
    outbound: Arc<BoundedOutbound>,
    scheduler: Arc<ConnectionSurfaceScheduler>,
}

fn mux(label: &str) -> Arc<Mux> {
    Mux::new_for_test(format!("app-children-{label}"), crate::SurfaceOptions::default())
}

fn connect(mux: &Arc<Mux>) -> Conn {
    let outbound = Arc::new(BoundedOutbound::default());
    let writer = MessageWriter::new(QueuedSink { outbound: outbound.clone(), control: None });
    let client = mux.control_clients.register(ClientTransport::Unix, writer.clone());
    let scheduler =
        Arc::new(ConnectionSurfaceScheduler::new(mux.surface_operation_admission.clone()));
    Conn { client, writer, outbound, scheduler }
}

fn app(mux: &Arc<Mux>) -> Conn {
    let conn = connect(mux);
    set_role_for_test(mux, conn.client, "main");
    set_peer_key_for_test(mux, conn.client, "token:100.1");
    set_verified_app_for_test(mux, conn.client, true);
    conn
}

fn send(mux: &Arc<Mux>, conn: &Conn, request: Value) -> Value {
    assert!(handle_connection_message(
        mux,
        conn.client,
        &request.to_string(),
        &conn.writer,
        &conn.scheduler
    ));
    reply(conn)
}

/// The connection's next reply (starts and reads answer from a worker).
fn reply(conn: &Conn) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(message) = conn.outbound.try_pop() {
            return serde_json::from_str(&message).unwrap();
        }
        assert!(Instant::now() < deadline, "no reply");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn shell(script: &str) -> Value {
    json!({
        "id": 1,
        "cmd": "app-child-start",
        "name": "test",
        "owner": "pane-1",
        "executable": "/bin/sh",
        "args": ["-c", script],
        "env": {"PATH": "/usr/bin:/bin"},
    })
}

fn started(mux: &Arc<Mux>, conn: &Conn, request: Value) -> (String, i32) {
    let answer = send(mux, conn, request);
    assert_eq!(answer["ok"], true, "{answer}");
    let child = answer["data"]["child"].as_str().unwrap().to_owned();
    let pid = i32::try_from(answer["data"]["pid"].as_i64().unwrap()).unwrap();
    (child, pid)
}

/// Every byte of `child`'s output up to its end, and how it ended.
fn read_to_end(mux: &Arc<Mux>, conn: &Conn, child: &str) -> (String, Value) {
    let mut output = String::new();
    let mut after = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let answer = send(
            mux,
            conn,
            json!({"id": 2, "cmd": "app-child-read", "child": child, "after": after, "wait_ms": 2000}),
        );
        assert_eq!(answer["ok"], true, "{answer}");
        output.push_str(answer["data"]["output"].as_str().unwrap());
        after = answer["data"]["next"].as_u64().unwrap();
        if let Some(exit) = answer["data"].get("exit") {
            return (output, exit.clone());
        }
        assert!(Instant::now() < deadline, "the child never ended: {output}");
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn listed(mux: &Arc<Mux>, conn: &Conn) -> Vec<Value> {
    let answer = send(mux, conn, json!({"id": 3, "cmd": "app-child-list"}));
    assert_eq!(answer["ok"], true, "{answer}");
    answer["data"]["children"].as_array().unwrap().clone()
}

#[test]
fn app_child_routes_reach_the_module() {
    let route = routes::find("app-child-start").expect("route");
    assert_eq!(route.stage, FrameStage::AppChildren);
    assert!(routes::find("app-child-release-all").is_some());
    assert!(routes::find("app-children").is_none());
}

#[test]
fn a_connection_that_is_not_the_verified_app_is_refused() {
    let mux = mux("refused");
    let agent = connect(&mux);
    let answer = send(&mux, &agent, shell("true"));
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error_code"], "app-child.forbidden");
    let unproven = connect(&mux);
    set_role_for_test(&mux, unproven.client, "main");
    let answer = send(&mux, &unproven, json!({"id": 1, "cmd": "app-child-list"}));
    assert_eq!(answer["error_code"], "app-child.forbidden", "{answer}");
    let relay = connect(&mux);
    set_role_for_test(&mux, relay.client, "page_relay");
    set_verified_app_for_test(&mux, relay.client, true);
    let answer = send(&mux, &relay, json!({"id": 1, "cmd": "app-child-list"}));
    assert_eq!(answer["error_code"], "app-child.forbidden", "{answer}");
}

#[test]
fn a_malformed_start_is_refused_before_any_spawn() {
    let mux = mux("malformed");
    let conn = app(&mux);
    let mut relative = shell("true");
    relative["executable"] = json!("sh");
    assert_eq!(send(&mux, &conn, relative)["error_code"], "app-child.bad-request");
    let mut secret = shell("true");
    secret["secret"] = json!(true);
    assert_eq!(send(&mux, &conn, secret)["error_code"], "app-child.bad-request");
    let mut env = shell("true");
    env["env"] = json!({"A=B": "c"});
    assert_eq!(send(&mux, &conn, env)["error_code"], "app-child.bad-request");
    assert!(listed(&mux, &conn).is_empty());
}

#[test]
fn a_command_streams_its_output_and_its_exit() {
    let mux = mux("output");
    let conn = app(&mux);
    let (child, _) = started(&mux, &conn, shell("echo one; sleep 0.2; echo two >&2; exit 7"));
    let (output, exit) = read_to_end(&mux, &conn, &child);
    assert_eq!(output, "one\ntwo\n");
    assert_eq!(exit, json!({"code": 7}));
}

#[test]
fn a_start_with_a_ready_line_answers_once_the_line_comes() {
    let mux = mux("ready");
    let conn = app(&mux);
    let mut request = shell(
        "echo starting; sleep 0.1; echo 'Web UI available at http://127.0.0.1:1/?tkn=x'; sleep 30",
    );
    request["ready"] = json!("Web UI available at ");
    let answer = send(&mux, &conn, request);
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["data"]["ready_line"], "Web UI available at http://127.0.0.1:1/?tkn=x");
    let child = answer["data"]["child"].as_str().unwrap();
    let rows = listed(&mux, &conn);
    assert_eq!(
        rows[0]["ready_line"], answer["data"]["ready_line"],
        "the relaunched app reads it again"
    );
    let stop = send(&mux, &conn, json!({"id": 4, "cmd": "app-child-stop", "child": child}));
    assert_eq!(stop["data"]["stopped"], true, "{stop}");
    let (_, exit) = read_to_end(&mux, &conn, child);
    assert_eq!(exit, json!({"signal": libc::SIGTERM}));
}

#[test]
fn a_child_that_exits_before_its_ready_line_fails_its_start() {
    let mux = mux("not-ready");
    let conn = app(&mux);
    let mut request = shell("echo nope; exit 3");
    request["ready"] = json!("listening");
    let answer = send(&mux, &conn, request);
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error_code"], "app-child.not-ready");
    let mut slow = shell("sleep 30");
    slow["ready"] = json!("listening");
    slow["ready_timeout_ms"] = json!(200);
    let answer = send(&mux, &conn, slow);
    assert_eq!(answer["error_code"], "app-child.not-ready", "{answer}");
    eventually("a child that missed its deadline is stopped", || {
        listed(&mux, &conn).iter().all(|row| row["running"] == false)
    });
}

#[test]
fn a_lifeline_child_gets_its_secret_on_stdin_and_ends_when_stopped() {
    let mux = mux("lifeline");
    let conn = app(&mux);
    let mut request =
        shell("read secret; echo \"listening ${#secret}\"; cat >/dev/null; echo closed");
    request["stdin"] = json!("lifeline");
    request["secret"] = json!(true);
    request["ready"] = json!("listening");
    let answer = send(&mux, &conn, request);
    assert_eq!(answer["ok"], true, "{answer}");
    let secret = answer["data"]["secret"].as_str().unwrap();
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(answer["data"]["ready_line"], "listening 64");
    assert_eq!(
        listed(&mux, &conn)[0]["secret"],
        secret,
        "the relaunched app gets the secret again"
    );
    let child = answer["data"]["child"].as_str().unwrap();
    // `terminate` closes the lifeline first: `cat` sees its end.
    send(&mux, &conn, json!({"id": 4, "cmd": "app-child-stop", "child": child}));
    let (output, _) = read_to_end(&mux, &conn, child);
    assert!(output.starts_with("listening 64\n"), "{output}");
}

#[test]
fn interrupt_reaches_the_whole_process_group() {
    let mux = mux("interrupt");
    let conn = app(&mux);
    let (child, pid) = started(&mux, &conn, shell("sleep 30 & wait"));
    eventually("the command runs", || alive(pid));
    let answer = send(
        &mux,
        &conn,
        json!({"id": 4, "cmd": "app-child-stop", "child": child, "mode": "interrupt"}),
    );
    assert_eq!(answer["data"]["stopped"], true);
    let (_, exit) = read_to_end(&mux, &conn, &child);
    assert!(exit.get("signal").is_some() || exit.get("code").is_some(), "{exit}");
    let unknown = send(&mux, &conn, json!({"id": 5, "cmd": "app-child-stop", "child": "nope"}));
    assert_eq!(unknown["error_code"], "app-child.unknown");
}

#[test]
fn a_child_outlives_the_app_connection_and_a_returning_app_adopts_it() {
    let mux = mux("adopt");
    mux.control_clients.app_children.set_grace_for_test(Duration::from_millis(300));
    let first = app(&mux);
    let (child, pid) = started(&mux, &first, shell("echo before; sleep 2; echo after"));
    // The app crashed: its connection ends with the child running.
    assert!(disconnect_client(&mux, first.client, false));
    assert!(alive(pid), "the child ended with the app's connection");
    let second = app(&mux);
    let rows = listed(&mux, &second);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["child"], child);
    assert_eq!(rows[0]["running"], true);
    // The grace passed with an app connected: the child keeps running.
    std::thread::sleep(Duration::from_millis(600));
    assert!(alive(pid), "an adopted child was ended at the grace");
    let (output, exit) = read_to_end(&mux, &second, &child);
    assert_eq!(output, "before\nafter\n", "the output continued across the app's crash");
    assert_eq!(exit, json!({"code": 0}));
}

#[test]
fn no_app_within_the_grace_ends_every_child() {
    let mux = mux("grace");
    mux.control_clients.app_children.set_grace_for_test(Duration::from_millis(200));
    let conn = app(&mux);
    let (_, pid) = started(&mux, &conn, shell("sleep 30"));
    assert!(disconnect_client(&mux, conn.client, false));
    eventually("a child no app adopted kept running past the grace", || !alive(pid));
}

#[test]
fn a_connection_that_is_not_the_app_starts_no_grace() {
    let mux = mux("other-leaves");
    mux.control_clients.app_children.set_grace_for_test(Duration::from_millis(100));
    let conn = app(&mux);
    let other = connect(&mux);
    let (_, pid) = started(&mux, &conn, shell("sleep 30"));
    assert!(disconnect_client(&mux, other.client, false));
    std::thread::sleep(Duration::from_millis(400));
    assert!(alive(pid), "an agent's disconnect ended the app's child");
    let answer = send(&mux, &conn, json!({"id": 9, "cmd": "app-child-release-all"}));
    assert_eq!(answer["data"]["stopped"], 1, "{answer}");
    eventually("release-all ends the child", || !alive(pid));
}

#[test]
fn utf8_complete_keeps_only_whole_characters() {
    assert_eq!(utf8_complete(b"abc"), 3);
    assert_eq!(utf8_complete("é".as_bytes()), 2);
    assert_eq!(utf8_complete(&"aé".as_bytes()[..2]), 1);
    assert_eq!(utf8_complete(&"a😀".as_bytes()[..4]), 1);
    assert_eq!(utf8_complete(b""), 0);
}

#[test]
fn an_exit_watch_answers_only_when_the_child_finished() {
    let mux = mux("exit-watch");
    let conn = app(&mux);
    let (child, _) = started(&mux, &conn, shell("echo out; sleep 0.5; exit 4"));
    let request = json!({
        "id": 6, "cmd": "app-child-read", "child": child, "after": u64::MAX, "until_exit": true,
    });
    assert!(handle_connection_message(
        &mux,
        conn.client,
        &request.to_string(),
        &conn.writer,
        &conn.scheduler
    ));
    std::thread::sleep(Duration::from_millis(200));
    assert!(conn.outbound.try_pop().is_none(), "the exit watch answered before the exit");
    let answer = reply(&conn);
    assert_eq!(answer["data"]["exit"], json!({"code": 4}), "{answer}");
    assert_eq!(answer["data"]["output"], "", "{answer}");
}

#[test]
fn release_all_ends_every_child_at_once_not_at_the_grace() {
    let mux = mux("release-all");
    mux.control_clients.app_children.set_grace_for_test(Duration::from_secs(120));
    let conn = app(&mux);
    let (_, first) = started(&mux, &conn, shell("sleep 60"));
    let (_, second) = started(&mux, &conn, shell("trap '' INT; sleep 60"));
    // The app's clean quit: release-all, then its connection ends.
    let answer = send(&mux, &conn, json!({"id": 9, "cmd": "app-child-release-all"}));
    assert_eq!(answer["data"]["stopped"], 2, "{answer}");
    assert!(disconnect_client(&mux, conn.client, false));
    let quit = Instant::now();
    eventually("release-all left an app child running", || !alive(first) && !alive(second));
    assert!(quit.elapsed() < Duration::from_secs(10), "the children waited for the grace");
}

#[test]
fn a_closed_connection_drops_its_exit_watches() {
    let mux = mux("watch-slots");
    let conn = app(&mux);
    let (child, _) = started(&mux, &conn, shell("sleep 60"));
    let watch = json!({
        "id": 6, "cmd": "app-child-read", "child": child, "after": u64::MAX, "until_exit": true,
    });
    // More app lives than there are read slots on the child, each one watching its exit.
    for _ in 0..20 {
        let life = app(&mux);
        assert!(handle_connection_message(
            &mux,
            life.client,
            &watch.to_string(),
            &life.writer,
            &life.scheduler
        ));
        assert!(disconnect_client(&mux, life.client, false));
    }
    assert!(handle_connection_message(
        &mux,
        conn.client,
        &watch.to_string(),
        &conn.writer,
        &conn.scheduler
    ));
    std::thread::sleep(Duration::from_millis(100));
    if let Some(message) = conn.outbound.try_pop() {
        panic!("the live app's exit watch was refused: {message}");
    }
    send(&mux, &conn, json!({"id": 9, "cmd": "app-child-release-all"}));
}

#[test]
fn a_read_after_half_a_character_waits_instead_of_spinning() {
    let mux = mux("partial-utf8");
    let conn = app(&mux);
    let (child, _) = started(&mux, &conn, shell("printf 'a\\303'; sleep 30"));
    let first = send(
        &mux,
        &conn,
        json!({"id": 2, "cmd": "app-child-read", "child": child, "after": 0, "wait_ms": 5000}),
    );
    assert_eq!(first["data"]["output"], "a", "{first}");
    assert_eq!(first["data"]["next"], 1, "{first}");
    let began = Instant::now();
    let second = send(
        &mux,
        &conn,
        json!({"id": 3, "cmd": "app-child-read", "child": child, "after": 1, "wait_ms": 400}),
    );
    assert!(
        began.elapsed() >= Duration::from_millis(350),
        "a read with only half a character after it answered at once: {second}"
    );
    assert_eq!(second["data"]["output"], "", "{second}");
    assert_eq!(second["data"]["next"], 1, "{second}");
    send(&mux, &conn, json!({"id": 9, "cmd": "app-child-release-all"}));
}

/// A start of `script` straight on the table (the request's worker step); its answer comes on the receiver.
fn start_direct(mux: &Arc<Mux>, script: &str) -> std::sync::mpsc::Receiver<Value> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let spec = super::children::Spec {
        name: "test".into(),
        owner: "pane-1".into(),
        executable: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: None,
        input: super::children::Input::Null,
        secret: false,
        ready: None,
        ready_wait: Duration::from_secs(5),
    };
    let reply = Box::new(move |result: Result<Value, super::children::ChildError>| {
        let _ = sender.send(result.expect("the start failed"));
    });
    mux.control_clients.app_children.start(mux, spec, reply);
    receiver
}

fn pid_of(started: &Value) -> i32 {
    i32::try_from(started["pid"].as_i64().unwrap()).unwrap()
}

#[test]
fn a_start_in_flight_when_the_app_leaves_still_gets_the_grace() {
    let mux = mux("in-flight");
    let children = mux.control_clients.app_children.clone();
    children.set_grace_for_test(Duration::from_millis(200));
    let conn = app(&mux);
    // The start took its slot; the app crashes before the child is in the table.
    children.reserve().unwrap();
    assert!(disconnect_client(&mux, conn.client, false));
    let pid =
        pid_of(&start_direct(&mux, "sleep 60").recv_timeout(Duration::from_secs(10)).unwrap());
    eventually("a child whose start was in flight outlived the grace", || !alive(pid));
}

#[test]
fn a_start_that_ends_with_no_app_connected_gets_the_grace() {
    let mux = mux("no-app");
    let children = mux.control_clients.app_children.clone();
    children.set_grace_for_test(Duration::from_millis(200));
    // The app's connection closed before the start reserved its slot: nothing ran then.
    children.reserve().unwrap();
    let pid =
        pid_of(&start_direct(&mux, "sleep 60").recv_timeout(Duration::from_secs(10)).unwrap());
    eventually("a child started with no app connected was never ended", || !alive(pid));
}

#[test]
fn the_daemons_end_kills_a_child_that_ignores_sigterm() {
    let mux = mux("daemon-end");
    let conn = app(&mux);
    let mut request = shell("trap '' TERM; echo ready; while :; do sleep 1; done");
    request["ready"] = json!("ready");
    let (_, pid) = started(&mux, &conn, request);
    let began = Instant::now();
    assert_eq!(mux.control_clients.app_children.end_within(Duration::from_millis(300)), 1);
    assert!(began.elapsed() < Duration::from_secs(5), "the daemon's end waited past its bound");
    // Well before the stop's own SIGKILL escalation (a continue item shutdown does not run).
    let deadline = Instant::now() + super::children::TERMINATE_GRACE / 2;
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "a child that ignores SIGTERM outlived the daemon's end"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
