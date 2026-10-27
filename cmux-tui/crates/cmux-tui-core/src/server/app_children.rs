//! App children (`app-children-v1`, p01-process-239): the long-lived
//! processes the cmux app used to start itself (a shell-mode command of the
//! agent pane, the VS Code `serve-web` server, the local remote-browser
//! host) run as children of this daemon instead, so an app crash or a
//! `kill -9` of the app leaves them running and the relaunched app finds
//! them again by `app-child-list`. Chrome keeps its long-lived children in
//! the browser process the same way (content/browser/
//! child_process_launcher.h): a renderer never owns a utility process.
//!
//! The lifeline rule. Only the verified app (a Unix connection whose hello
//! declared role `main` and passed its proof) starts, reads, stops or lists
//! children; they belong to the app, not to one connection. When the last
//! verified app connection ends, a grace of [`APP_GRACE`] starts: an app
//! that comes back and calls any `app-child-*` command in it adopts the
//! children; else, or when no app is connected when it ends, every child is
//! terminated. A clean quit ends them at once (`app-child-release-all`), and
//! the daemon's own end terminates every child. A child started with
//! `stdin: "lifeline"` also sees its stdin end when the daemon dies.
//!
//! No thread waits for a child: its spawn runs on a request-wait work item,
//! its output on the daemon's read loop (`mux-terminal-io`), its exit on
//! the daemon's reaper (`mux-reaper`), and every deadline (a read's wait,
//! the ready deadline, a stop's escalation, the grace) is a posted work item.
//!
//! Wire (JSON lines; cmux-tui/spec/commands.md "App children"):
//! - `app-child-start {id, name, owner?, executable, args?, env?, cwd?,
//!   stdin?, secret?, ready?, ready_timeout_ms?}` answers `{child, pid,
//!   secret?, ready_line?}`.
//! - `app-child-read {id, child, after?, wait_ms?, until_exit?}` answers
//!   `{output, next, truncated?, exit?}`; `until_exit` answers only once the
//!   child finished, with no deadline.
//! - `app-child-stop {id, child, mode?}` answers `{stopped}`.
//! - `app-child-list {id, owner?}` answers `{children}`.
//! - `app-child-release-all {id}` answers `{stopped}`.

mod children;
mod output;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use cmux_threads::work::{Lane, Priority, Shutdown};
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub(crate) use children::AppChildren;
use children::{ChildError, Input, ReadWait, Spec, StopMode};

use super::client_registry::ClientRecord;
use super::routes::{Route, RouteLine, RouteName};
use super::{ClientTransport, FrameStage, MessageWriter, Response, send_response};
use crate::mux::Mux;
use crate::request_origin::HelloRole;

pub(crate) const CAPABILITY: &str = "app-children-v1";
/// How long the children wait for an app after the last one left.
pub(crate) const APP_GRACE: Duration = Duration::from_secs(30);
/// The default ready deadline (a first `serve-web` run downloads the server).
const DEFAULT_READY_WAIT: Duration = Duration::from_secs(60);
const MAX_NAME: usize = 128;
const MAX_OWNER: usize = 256;
const MAX_READY: usize = 256;
const MAX_ARGS: usize = 256;
const MAX_ENV: usize = 1024;
/// Bytes of the arguments and the environment together.
const MAX_COMMAND_BYTES: usize = 1 << 20;

#[derive(Deserialize)]
struct Request {
    id: Option<Value>,
    #[serde(flatten)]
    command: ChildCommand,
}

#[derive(Deserialize)]
#[serde(tag = "cmd")]
enum ChildCommand {
    #[serde(rename = "app-child-start")]
    Start(StartParams),
    #[serde(rename = "app-child-read")]
    Read {
        child: String,
        #[serde(default)]
        after: u64,
        #[serde(default)]
        wait_ms: u64,
        #[serde(default)]
        until_exit: bool,
    },
    #[serde(rename = "app-child-stop")]
    Stop {
        child: String,
        #[serde(default)]
        mode: Option<String>,
    },
    #[serde(rename = "app-child-list")]
    List {
        #[serde(default)]
        owner: Option<String>,
    },
    #[serde(rename = "app-child-release-all")]
    ReleaseAll,
}

#[derive(Deserialize)]
struct StartParams {
    name: String,
    #[serde(default)]
    owner: String,
    executable: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Map<String, Value>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    secret: bool,
    #[serde(default)]
    ready: Option<String>,
    #[serde(default)]
    ready_timeout_ms: Option<u64>,
}

impl StartParams {
    /// The spec, or why the request is malformed. Reads no disk.
    fn spec(self) -> Result<Spec, &'static str> {
        let StartParams {
            name,
            owner,
            executable,
            args,
            env,
            cwd,
            stdin,
            secret,
            ready,
            ready_timeout_ms,
        } = self;
        if name.is_empty() || name.len() > MAX_NAME || owner.len() > MAX_OWNER {
            return Err("name must be 1-128 bytes and owner at most 256");
        }
        if !executable.starts_with('/') || executable.contains('\0') {
            return Err("executable must be an absolute path");
        }
        if cwd.as_deref().is_some_and(|cwd| !cwd.starts_with('/') || cwd.contains('\0')) {
            return Err("cwd must be an absolute path");
        }
        if args.len() > MAX_ARGS || env.len() > MAX_ENV {
            return Err("too many arguments or environment entries");
        }
        let mut variables = Vec::with_capacity(env.len());
        for (key, value) in env {
            let Value::String(value) = value else {
                return Err("environment values must be strings");
            };
            if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
                return Err("malformed environment entry");
            }
            variables.push((key, value));
        }
        let bytes = args.iter().map(String::len).sum::<usize>()
            + variables.iter().map(|(key, value)| key.len() + value.len()).sum::<usize>();
        if bytes > MAX_COMMAND_BYTES || args.iter().any(|arg| arg.contains('\0')) {
            return Err("arguments too long or malformed");
        }
        let input = match stdin.as_deref() {
            None | Some("null") => Input::Null,
            Some("lifeline") => Input::Lifeline,
            Some(_) => return Err("stdin must be null or lifeline"),
        };
        if secret && input != Input::Lifeline {
            return Err("a secret needs stdin lifeline");
        }
        if ready.as_deref().is_some_and(|ready| ready.is_empty() || ready.len() > MAX_READY) {
            return Err("ready must be 1-256 bytes");
        }
        Ok(Spec {
            name,
            owner,
            executable: PathBuf::from(executable),
            args,
            env: variables,
            cwd: cwd.map(PathBuf::from),
            input,
            secret,
            ready,
            ready_wait: ready_timeout_ms.map_or(DEFAULT_READY_WAIT, Duration::from_millis),
        })
    }
}

/// The `cmd` names this module answers (server/routes.rs).
pub(in crate::server) const ROUTES: &[Route] = &[Route {
    names: &[RouteName::Prefix("app-child-")],
    stage: FrameStage::AppChildren,
    entry: handle,
}];

/// Whether `record` is the verified app: a Unix connection whose hello
/// declared role `main` and passed its proof (browser_host_command.rs).
pub(super) fn is_app(record: &ClientRecord) -> bool {
    matches!(record.transport, ClientTransport::Unix)
        && record.origin.role == HelloRole::Main
        && record.origin.verified_app
}

fn is_app_client(mux: &Mux, client: u64) -> bool {
    let state = mux.control_clients.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.clients.get(&client).is_some_and(is_app)
}

fn app_connected(mux: &Mux) -> bool {
    let state = mux.control_clients.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.clients.values().any(is_app)
}

impl AppChildren {
    /// The last verified app connection ended: the grace starts, and at its
    /// end the children are terminated unless an app adopted them.
    pub(super) fn last_app_left(self: &Arc<Self>) {
        let Some(mux) = self.mux() else { return };
        if !self.any_running() {
            return;
        }
        let (generation, grace) = self.begin_grace(APP_GRACE);
        let children = Arc::downgrade(self);
        let weak_mux = Arc::downgrade(&mux);
        let posted = mux.work.post_delayed(
            grace,
            Priority::BestEffort,
            Lane::Cpu,
            Shutdown::ContinueOnShutdown,
            move || {
                let (Some(children), Some(mux)) = (children.upgrade(), weak_mux.upgrade()) else {
                    return;
                };
                if children.grace_current(generation) && !app_connected(&mux) {
                    let ended = children.terminate_all();
                    eprintln!(
                        "cmux-tui: no app came back within the grace; {ended} app children end"
                    );
                }
            },
        );
        if posted.is_err() {
            // Shutdown began: the daemon's end terminates them.
            self.terminate_all();
        }
    }
}

/// The route's typed handler for an `app-child-*` line on the
/// connection's reader thread. A start runs on a request-wait item; the
/// others answer at once or park a reply that output, an exit or a posted
/// deadline sends. A line that is none of the commands is left to the
/// `Command` parse (`None`).
fn handle(line: &RouteLine<'_>) -> Option<bool> {
    let RouteLine { mux, client, message, writer, .. } = *line;
    let Request { id, command } = crate::line_parses::parse(message).ok()?;
    if !is_app_client(mux, client) {
        return Some(fail(
            writer,
            id,
            "app-child.forbidden",
            "app children need the verified cmux app connection",
        ));
    }
    let children = mux.control_clients.app_children.clone();
    // Any command of the app adopts the children: the grace ends.
    children.end_grace();
    Some(match command {
        ChildCommand::Start(params) => start(mux, &children, id, params, writer),
        ChildCommand::Read { child, after, wait_ms, until_exit } => {
            let reply_writer = writer.clone();
            let reply_id = id.clone();
            let wait = if until_exit {
                ReadWait::UntilExit
            } else {
                ReadWait::For(Duration::from_millis(wait_ms))
            };
            let reply = Box::new(move |chunk: children::Chunk| {
                ok(&reply_writer, reply_id, chunk.json());
            });
            match children.read(mux, &child, after, wait, reply) {
                Ok(()) => true,
                Err(error) => fail_with(writer, id, &error),
            }
        }
        ChildCommand::Stop { child, mode } => {
            let mode = match mode.as_deref() {
                None | Some("terminate") => StopMode::Terminate,
                Some("interrupt") => StopMode::Interrupt,
                Some("hangup") => StopMode::Hangup,
                Some(_) => {
                    return Some(fail(
                        writer,
                        id,
                        "app-child.bad-request",
                        "mode must be terminate, interrupt or hangup",
                    ));
                }
            };
            if children.stop(&child, mode) {
                ok(writer, id, json!({"stopped": true}))
            } else {
                fail_with(writer, id, &ChildError::Unknown)
            }
        }
        ChildCommand::List { owner } => {
            ok(writer, id, json!({"children": children.list(owner.as_deref())}))
        }
        ChildCommand::ReleaseAll => {
            let stopped = children.terminate_all();
            ok(writer, id, json!({"stopped": stopped}))
        }
    })
}

fn start(
    mux: &Arc<Mux>,
    children: &Arc<AppChildren>,
    id: Option<Value>,
    params: StartParams,
    writer: &MessageWriter,
) -> bool {
    let spec = match params.spec() {
        Ok(spec) => spec,
        Err(message) => return fail(writer, id, "app-child.bad-request", message),
    };
    if let Err(error) = children.reserve() {
        return fail_with(writer, id, &error);
    }
    let (worker_mux, worker_children) = (mux.clone(), children.clone());
    let (reply_writer, reply_id) = (writer.clone(), id.clone());
    // The spawn may take hundreds of ms under memory pressure, and a start
    // with a ready line waits for it (`request_wait.rs`).
    let posted = mux.post_request_wait(move || {
        let reply = Box::new(move |result: Result<Value, ChildError>| match result {
            Ok(data) => {
                ok(&reply_writer, reply_id, data);
            }
            Err(error) => {
                fail_with(&reply_writer, reply_id, &error);
            }
        });
        worker_children.start(&worker_mux, spec, reply);
    });
    if posted.is_err() {
        children.unreserve();
        return fail_with(writer, id, &ChildError::ShuttingDown);
    }
    true
}

fn ok(writer: &MessageWriter, id: Option<Value>, data: Value) -> bool {
    send_response(
        writer,
        Response {
            id,
            ok: true,
            data: Some(data),
            error: None,
            error_code: None,
            error_delivery: None,
        },
    )
}

fn fail_with(writer: &MessageWriter, id: Option<Value>, error: &ChildError) -> bool {
    fail(writer, id, error.code(), &error.message())
}

fn fail(writer: &MessageWriter, id: Option<Value>, code: &str, message: &str) -> bool {
    send_response(
        writer,
        Response {
            id,
            ok: false,
            data: None,
            error: Some(message.to_string()),
            error_code: Some(code.to_string()),
            error_delivery: None,
        },
    )
}

#[cfg(test)]
#[path = "app_children_tests.rs"]
mod tests;
