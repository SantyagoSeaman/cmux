//! New terminals run on the holder, and a terminal host process per
//! terminal (`__terminal-host`) starts only with the opt-out
//! `CMUX_TUI_TERMINAL_HOST_MODEL=hosts` (p01-process-138), across create,
//! split and restore.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const MODEL_ENV: &str = "CMUX_TUI_TERMINAL_HOST_MODEL";

struct Daemon {
    child: Option<Child>,
    /// Every pid the daemon ran as: a host's owner mark names one.
    pids: Vec<u32>,
    env: Vec<(&'static str, &'static str)>,
    dir: PathBuf,
    socket: PathBuf,
    /// Shells and holders this test made, ended at drop: each outlives a
    /// killed daemon on purpose.
    cleanup: Vec<i32>,
}

impl Daemon {
    /// A daemon whose terminal model env is only `env`.
    fn start(name: &str, env: &[(&'static str, &'static str)]) -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = PathBuf::from("/tmp")
            .join(format!("cmux-host-model-{name}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut daemon = Self {
            child: None,
            pids: Vec::new(),
            env: env.to_vec(),
            socket: dir.join("mux.sock"),
            dir,
            cleanup: Vec::new(),
        };
        daemon.restart();
        daemon
    }

    fn restart(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let child = Command::new(env!("CARGO_BIN_EXE_cmux-tui"))
            .args(["--headless", "--session", "host-model", "--socket"])
            .arg(&self.socket)
            .arg("--state")
            .arg(self.dir.join("state"))
            .env("CMUX_TUI_CONFIG", self.dir.join("config.json"))
            .env_remove(MODEL_ENV)
            .env_remove("CMUX_ENABLE_FEATURES")
            .env_remove("CMUX_DISABLE_FEATURES")
            .envs(self.env.iter().copied())
            .env("SHELL", "/bin/sh")
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_CLIENT")
            .env_remove("SSH_TTY")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        self.pids.push(child.id());
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(60);
        while UnixStream::connect(&self.socket).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn sigkill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn request(&self, mut body: Value) -> Value {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        body["id"] = json!(1);
        (&stream).write_all(format!("{body}\n").as_bytes()).unwrap();
        let mut reader = BufReader::new(&stream);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0, "no reply to {body}");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == json!(1) && reply.get("ok").is_some() {
                assert_eq!(reply["ok"], true, "{body} failed: {reply}");
                return reply["data"].clone();
            }
        }
    }

    /// A new workspace running `cat`: its surface and terminal id.
    fn create(&mut self, name: &str) -> (u64, String) {
        let created = self.request(json!({"cmd": "run", "argv": ["/bin/cat"],
            "new_workspace": true, "name": name, "cols": 80, "rows": 24}));
        self.started(&created)
    }

    /// A split of the pane that shows `surface`: its surface and terminal id.
    fn split(&mut self, surface: u64) -> (u64, String) {
        let pane = self.pane_of(surface);
        let split = self
            .request(json!({"cmd": "split", "pane": pane, "dir": "right", "cols": 40, "rows": 24}));
        self.started(&split)
    }

    fn started(&mut self, reply: &Value) -> (u64, String) {
        let surface = reply["surface"].as_u64().unwrap_or_else(|| panic!("no surface: {reply}"));
        let terminal_id =
            reply["terminal_id"].as_str().unwrap_or_else(|| panic!("no terminal id: {reply}"));
        self.track(surface);
        (surface, terminal_id.to_string())
    }

    fn pane_of(&self, surface: u64) -> Value {
        let tree = self.request(json!({"cmd": "list-workspaces"}));
        let panes = tree["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|workspace| workspace["screens"].as_array().into_iter().flatten())
            .flat_map(|screen| screen["panes"].as_array().into_iter().flatten());
        for pane in panes {
            let shows = pane["tabs"].as_array().into_iter().flatten().any(|tab| {
                *tab == json!(surface)
                    || tab["id"] == json!(surface)
                    || tab["surface"] == json!(surface)
            });
            if shows {
                return pane["id"].clone();
            }
        }
        panic!("no pane shows surface {surface}: {tree}");
    }

    /// End `surface`'s shell and its parent (the holder or its host) at drop.
    fn track(&mut self, surface: u64) {
        let resources = self.request(json!({"cmd": "terminal-resources", "surfaces": [surface]}));
        let shell = resources["terminals"][0]["pid"]
            .as_i64()
            .unwrap_or_else(|| panic!("no pid: {resources}")) as i32;
        for pid in [shell, parent_of(shell)] {
            if pid > 1 && !self.cleanup.contains(&pid) {
                self.cleanup.push(pid);
            }
        }
    }

    /// The live surface of `terminal_id` after a restart, once it runs.
    fn running_surface(&self, terminal_id: &str) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let resolved =
                self.request(json!({"cmd": "resolve-terminal", "terminal_id": terminal_id}));
            if resolved["lifecycle"] == "running"
                && let Some(surface) = resolved["surface"].as_u64()
            {
                return surface;
            }
            assert!(Instant::now() < deadline, "{terminal_id} is not running: {resolved}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `text` typed into `surface` shows on its screen: the terminal runs.
    fn echoes(&self, surface: u64, text: &str) {
        self.request(json!({"cmd": "send", "surface": surface, "text": format!("{text}\n")}));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let screen = self.request(json!({"cmd": "read-screen", "surface": surface}));
            if screen["text"].as_str().is_some_and(|screen| screen.contains(text)) {
                return;
            }
            assert!(Instant::now() < deadline, "surface {surface} never showed {text:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Terminal host records in the session's state.
    fn host_records(&self) -> Vec<PathBuf> {
        walk(&self.dir.join("state"))
            .into_iter()
            .filter(|path| {
                path.parent().is_some_and(|dir| dir.to_string_lossy().contains("terminal-hosts"))
                    && path.extension().is_some_and(|ext| ext == "json")
            })
            .collect()
    }

    /// Live `__terminal-host` processes (spares too, which write no record)
    /// whose owner mark names one of this daemon's pids: `pid args` lines.
    fn host_processes(&self) -> Vec<String> {
        let listing = Command::new("ps").args(["-A", "-ww", "-o", "pid=,args="]).output().unwrap();
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.contains("__terminal-host"))
            .filter(|line| self.pids.iter().any(|pid| owned_by(line, *pid)))
            .map(|line| line.trim().to_string())
            .collect()
    }

    fn assert_no_terminal_host(&self, step: &str) {
        let (records, processes) = (self.host_records(), self.host_processes());
        assert!(
            records.is_empty() && processes.is_empty(),
            "{step} started a terminal host: records {records:?}, processes {processes:?}"
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let hosts: Vec<i32> = self
            .host_processes()
            .iter()
            .filter_map(|line| line.split_whitespace().next()?.parse().ok())
            .collect();
        self.sigkill();
        for pid in self.cleanup.drain(..).chain(hosts) {
            // SAFETY: SIGKILL of a shell, holder or host this test's daemon
            // started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A host's command line carries `--owner <bundle>:<tag>@<daemon pid>`.
fn owned_by(args: &str, pid: u32) -> bool {
    let mark = format!("@{pid}");
    args.split_whitespace().any(|word| word.ends_with(&mark))
}

fn parent_of(pid: i32) -> i32 {
    let out = Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                paths.push(path);
            }
        }
    }
    paths
}

/// Create, split, restore (a daemon SIGKILL and a restart on the same
/// state) and create again: every terminal runs and takes input, and no
/// step starts a terminal host process.
fn starts_no_terminal_host(name: &str, env: &[(&'static str, &'static str)]) {
    let mut daemon = Daemon::start(name, env);
    let (created, created_id) = daemon.create("created");
    daemon.echoes(created, "create-ok");
    daemon.assert_no_terminal_host("create");

    let (split, split_id) = daemon.split(created);
    daemon.echoes(split, "split-ok");
    daemon.assert_no_terminal_host("split");

    daemon.sigkill();
    daemon.restart();
    for (terminal_id, text) in [(&created_id, "restored-1"), (&split_id, "restored-2")] {
        let surface = daemon.running_surface(terminal_id);
        daemon.echoes(surface, text);
    }
    daemon.assert_no_terminal_host("restore");

    let (after, _) = daemon.create("after-restore");
    daemon.echoes(after, "after-restore-ok");
    daemon.assert_no_terminal_host("create after restore");
}

/// The acceptance: the default env, which is the holder model once
/// p01-process-002 makes the holder the default.
#[test]
fn the_default_env_starts_no_terminal_host_across_create_split_and_restore() {
    starts_no_terminal_host("default", &[]);
}

/// The holder model named through its feature list.
#[test]
fn the_holder_model_starts_no_terminal_host_across_create_split_and_restore() {
    starts_no_terminal_host("holder", &[("CMUX_ENABLE_FEATURES", "terminalHolder")]);
}

/// The opt-out still reaches the hosted path: a host for the terminal.
#[test]
fn the_hosts_opt_out_still_starts_a_terminal_host() {
    let mut daemon = Daemon::start("hosts", &[(MODEL_ENV, "hosts")]);
    let (created, created_id) = daemon.create("hosted");
    daemon.echoes(created, "hosted-ok");
    let records = daemon.host_records();
    let names: Vec<_> = records.iter().filter_map(|path| path.file_stem()?.to_str()).collect();
    assert!(names.contains(&created_id.as_str()), "no host record for {created_id}: {names:?}");
    assert!(!daemon.host_processes().is_empty(), "no __terminal-host process");
}
