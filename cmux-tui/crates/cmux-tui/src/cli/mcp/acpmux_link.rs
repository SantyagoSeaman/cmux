//! The one acpmux daemon connection of a `cmux mcp serve` server (the
//! `agents_dispatch` family, `agent_dispatch_tools.rs`): made at its first
//! call, kept for every call after it, and made again only when the daemon
//! closed it or the caller changed. Before, each call built a runtime and a
//! new daemon connection (acpmux-audit finding 10). Chromium keeps one Mojo
//! pipe to a service for the client's life the same way, and binds again
//! only after a disconnect (`mojo/public/cpp/bindings/remote.h`).

#[cfg(unix)]
use acpmux::client::{Client, ClientCaller};
#[cfg(unix)]
use std::sync::Arc;

/// The runtime and connection, both made on first use.
#[derive(Default)]
pub(super) struct AcpmuxLink {
    #[cfg(unix)]
    runtime: Option<tokio::runtime::Runtime>,
    #[cfg(unix)]
    client: Option<(ClientCaller, Arc<Client>)>,
    /// Connections made so far (a test reads it).
    #[cfg(unix)]
    connects: u64,
}

#[cfg(unix)]
impl AcpmuxLink {
    /// The one-thread runtime the connection's reader and writer run on,
    /// only while a call waits in it.
    fn runtime(&mut self) -> std::io::Result<&tokio::runtime::Runtime> {
        if self.runtime.is_none() {
            self.runtime =
                Some(tokio::runtime::Builder::new_current_thread().enable_all().build()?);
        }
        // Set just above when it was missing.
        self.runtime.as_ref().ok_or_else(|| std::io::Error::other("no runtime"))
    }

    /// Runs `work` on the link's runtime until it ends. Blocks: a tool call's
    /// thread (`Server::call`), never a runtime thread.
    pub(super) fn block_on<T>(&mut self, work: impl Future<Output = T>) -> std::io::Result<T> {
        Ok(self.runtime()?.block_on(work))
    }

    /// The connection for `caller`: the kept one while it is open and for the
    /// same caller, else a new one (`connect` makes it on the runtime).
    pub(super) fn client<F, Fut>(
        &mut self,
        caller: ClientCaller,
        connect: F,
    ) -> Result<Arc<Client>, String>
    where
        F: FnOnce(ClientCaller) -> Fut,
        Fut: Future<Output = anyhow::Result<Arc<Client>>>,
    {
        // The reader runs only while a call waits on the runtime: let it read
        // what came since the last call (a close among it) before the reuse.
        self.block_on(async {
            for _ in 0..2 {
                tokio::task::yield_now().await;
            }
        })
        .map_err(|error| error.to_string())?;
        if let Some((kept, client)) = &self.client
            && *kept == caller
            && !client.is_closed()
        {
            return Ok(client.clone());
        }
        self.client = None;
        let client = self
            .block_on(connect(caller.clone()))
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        self.connects += 1;
        self.client = Some((caller, client.clone()));
        Ok(client)
    }

    /// Connections made so far.
    #[cfg(test)]
    pub(super) fn connects(&self) -> u64 {
        self.connects
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    /// A stand-in daemon on `path`: it answers every request line with an
    /// empty result, and closes a connection after its `close_after`-th
    /// request (then says so on `closed`). Returns how many connections it
    /// took, once `rounds` ended.
    fn daemon(
        path: &Path,
        close_after: usize,
        rounds: usize,
        closed: std::sync::mpsc::Sender<()>,
    ) -> std::thread::JoinHandle<usize> {
        let listener = UnixListener::bind(path).expect("bind");
        // supervise-allow: the test joins it and reads its count
        std::thread::spawn(move || {
            let mut accepted = 0;
            for _ in 0..rounds {
                let Ok((stream, _)) = listener.accept() else { break };
                accepted += 1;
                let mut out = stream.try_clone().expect("clone");
                let mut served = 0;
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { break };
                    let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
                    let reply = json!({"jsonrpc": "2.0", "id": request["id"], "result": {}});
                    if writeln!(out, "{reply}").is_err() {
                        break;
                    }
                    served += 1;
                    if served == close_after {
                        let _ = out.shutdown(std::net::Shutdown::Both);
                        let _ = closed.send(());
                        break;
                    }
                }
            }
            accepted
        })
    }

    fn socket(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("cmux-link-{tag}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// One call: the kept connection, then a request on it.
    fn call(link: &mut AcpmuxLink, path: &Path, caller: &ClientCaller) -> Result<Value, String> {
        let path = path.to_owned();
        let client = link.client(caller.clone(), |caller| async move {
            Client::connect_as(&path, caller).await
        })?;
        link.block_on(async move { client.request("_acpmux/status", json!({})).await })
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }

    /// acpmux-audit finding 10: every tool call after the first goes over
    /// the first call's connection (0 connections per call), and a closed
    /// connection is made again at the next call.
    #[test]
    fn calls_share_one_connection_until_the_daemon_closes_it() {
        let path = socket("share");
        // The initialize and 10 requests on the first connection, then it closes.
        let (closed_tx, closed) = std::sync::mpsc::channel();
        let server = daemon(&path, 11, 2, closed_tx);
        let mut link = AcpmuxLink::default();
        let caller = ClientCaller::default();
        for _ in 0..10 {
            call(&mut link, &path, &caller).expect("a call");
        }
        assert_eq!(link.connects(), 1, "one connection for ten calls");
        // The daemon closed it after the tenth: the next call connects again.
        closed.recv().expect("the daemon closed the connection");
        call(&mut link, &path, &caller).expect("a call after the close");
        assert_eq!(link.connects(), 2);
        drop(link);
        assert_eq!(server.join().expect("daemon"), 2);
        let _ = std::fs::remove_file(&path);
    }
}
