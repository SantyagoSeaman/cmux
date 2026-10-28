//! Part of `Hub`; see `hub/mod.rs`. `_acpmux/session/auth` (Leo: "you gotta re-authenticate.
//! You gotta log in again on the modal picker before the user even sends a prompt."): what this
//! daemon knows of a chat's sign-in, from evidence in this order, the first that says something
//! answering:
//!
//! 1. The peer link (`server/session_auth.rs`): a peer's chat whose link is down is
//!    `peerUnreachable`.
//! 2. The chat's bound route: with `refresh` a new probe (never of a route whose secret is in
//!    the keychain), else its last answer at most [`PROBE_FRESH_MS`] old (`route_probe.rs`).
//!    `auth_needed` is `routeAuthNeeded`, `unreachable` `routeUnreachable`; `rate_limited` and
//!    `unknown` say nothing about the sign-in and fall through.
//! 3. The chat's newest turn ([`turn_evidence`]): a sign-in failure (`auth.signed_out`,
//!    `auth.expired`) or a completed turn. A target (a chat with no session yet) takes the
//!    newest such turn of a chat here with the same harness, account home and route.
//! 4. With `refresh`, the harness's own check that reads no keychain
//!    (`harness_health::fresh_auth`).
//! 5. Else `unknown`, never signed out. A connection error is never a sign-in state.
//!
//! Never the saved harness health (checked every six hours), and never the login keychain.

use super::*;

use crate::harness_health::Auth;
use crate::protocol::auth::{
    SessionAuthLogin, SessionAuthLoginKind as LoginKind, SessionAuthResult,
    SessionAuthSource as Source, SessionAuthState as State, SessionAuthTarget,
};
use crate::{route_account, routes};
use std::collections::BTreeMap;

/// Listed in `initialize`'s features: this daemon answers `_acpmux/session/auth`.
pub(crate) const FEATURE: &str = "sessionAuth";
/// A route probe answer younger than this is reused (as long as `route_probe.rs` keeps one).
pub(crate) const PROBE_FRESH_MS: u64 = crate::route_probe::KEPT_MS;

/// A chat's sign-in as one of its turns showed it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnEvidence {
    pub state: State,
    pub at: u64,
    pub detail: String,
}

/// What a chat's `lastTurn` says of its sign-in: `ready` when it completed, `signedOut` or
/// `expired` when it failed on its sign-in (its `errorKind`; a turn recorded before the kinds
/// existed, by its error text). A cancelled turn, or one that failed another way (a connection
/// error), says nothing.
pub(crate) fn turn_evidence(last: &Value) -> Option<TurnEvidence> {
    let at = last.get("endedAt").and_then(Value::as_u64)?;
    let text = last.get("errorText").and_then(Value::as_str).unwrap_or_default();
    let state = match last.get("status").and_then(Value::as_str)? {
        "completed" => State::Ready,
        "failed" => {
            let kind = last.get("errorKind").and_then(Value::as_str);
            match kind.or_else(|| crate::recovery::auth_kind(text))? {
                crate::recovery::kind::AUTH_SIGNED_OUT => State::SignedOut,
                crate::recovery::kind::AUTH_EXPIRED => State::Expired,
                _ => return None,
            }
        }
        _ => return None,
    };
    let detail = match state {
        State::Ready => "its last turn completed".to_owned(),
        _ => format!("its last turn failed: {text}"),
    };
    Some(TurnEvidence { state, at, detail })
}

/// The evidence one answer is decided from, in the module's order.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    pub harness: String,
    pub route_id: Option<String>,
    /// The route probe's answer and when it came.
    pub probe: Option<(u64, Value)>,
    /// The turn and the chat it came from.
    pub turn: Option<(String, TurnEvidence)>,
    /// The harness's own check and when it ran; Err: it could not answer.
    pub check: Option<(u64, Result<(Auth, String), String>)>,
    /// The harness's login command, for `login`.
    pub login: Option<String>,
}

impl Facts {
    /// Whether the evidence before the harness check already answers.
    fn settled(&self) -> bool {
        self.route_state().is_some() || self.turn.is_some()
    }

    fn route_state(&self) -> Option<State> {
        match self.probe.as_ref()?.1.get("status").and_then(Value::as_str)? {
            "auth_needed" => Some(State::RouteAuthNeeded),
            "unreachable" => Some(State::RouteUnreachable),
            _ => None,
        }
    }

    /// The answer: the first evidence that says something, else `unknown`; its `detail` one
    /// sentence ([`one_sentence`]).
    pub(crate) fn decide(self) -> SessionAuthResult {
        let mut out = self.answer();
        out.detail = one_sentence(&out.detail);
        out
    }

    fn answer(self) -> SessionAuthResult {
        let route = self.route_id.clone();
        let mut out = SessionAuthResult {
            state: State::Unknown,
            source: Source::None,
            evidence_at: None,
            detail: "nothing here shows this chat's sign-in yet".into(),
            harness: self.harness.clone(),
            host: None,
            route_id: route.clone(),
            login: None,
            from_session_id: None,
        };
        let terminal = |command: &Option<String>| SessionAuthLogin {
            kind: LoginKind::Terminal,
            command: command.clone(),
            host: None,
            route_id: None,
        };
        if let (Some(state), Some((at, probe))) = (self.route_state(), &self.probe) {
            let message = probe.get("message").and_then(Value::as_str).unwrap_or_default();
            out.state = state;
            out.source = Source::RouteProbe;
            out.evidence_at = Some(*at);
            out.detail = format!("route {} answered: {message}", route.as_deref().unwrap_or("?"));
            if state == State::RouteAuthNeeded {
                out.login = Some(SessionAuthLogin {
                    kind: LoginKind::Route,
                    command: None,
                    host: None,
                    route_id: route,
                });
            }
            return out;
        }
        if let Some((from, turn)) = self.turn {
            out.state = turn.state;
            out.source = Source::Turn;
            out.evidence_at = Some(turn.at);
            out.detail = turn.detail;
            out.from_session_id = Some(from);
            if turn.state != State::Ready {
                out.login = Some(terminal(&self.login));
            }
            return out;
        }
        match self.check {
            // A check that ran is the evidence, whatever it said; one that could not reach its
            // provider says unknown, never signed out.
            Some((at, Ok((auth, detail)))) => {
                out.state = match auth {
                    Auth::SignedIn => State::Ready,
                    Auth::Expired => State::Expired,
                    Auth::Unknown => State::Unknown,
                    _ => State::SignedOut,
                };
                out.source = Source::AuthCheck;
                out.evidence_at = Some(at);
                out.detail = detail;
                if matches!(out.state, State::SignedOut | State::Expired) {
                    out.login = Some(terminal(&self.login));
                }
            }
            Some((_, Err(why))) => out.detail = format!("the harness's sign-in is unknown: {why}"),
            _ => {}
        }
        out
    }
}

/// `text` as the one sentence a reply's `detail` is: its words on one line, up to the end of
/// its first sentence, at most [`DETAIL_CHARS`] characters. An agent's error text may run to
/// many lines.
pub(crate) fn one_sentence(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // A sentence ends at `.`, `!` or `?` before a space, but not after an abbreviation
    // ("e.g.", "etc.") or a word of one or two letters ("a.", "no.").
    // A closing quote or bracket right after the mark ends with it ("expired." Please).
    let closers = ['"', '\'', '\u{201d}', '\u{2019}', ')', ']'];
    let end = flat.match_indices(['.', '!', '?']).map(|(i, _)| i + 1).find_map(|i| {
        let word = flat[..i].rsplit(' ').next().unwrap_or_default().to_lowercase();
        let word = word.trim_start_matches(|c: char| !c.is_alphanumeric());
        let short = word.chars().count() <= 2;
        let rest = flat[i..].trim_start_matches(closers);
        let end = flat.len() - rest.len();
        (rest.starts_with(' ')
            && !short
            && !["e.g.", "i.e.", "etc.", "vs.", "cf.", "approx."].contains(&word))
        .then_some(end)
    });
    let sentence = end.map_or(flat.as_str(), |i| &flat[..i]);
    if sentence.chars().count() <= DETAIL_CHARS {
        return sentence.to_owned();
    }
    let cut: String = sentence.chars().take(DETAIL_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The longest `detail`, in characters.
const DETAIL_CHARS: usize = 200;

/// `VAR=<home> <command>` when the chat runs in an account home of its own.
fn login_command(command: Option<String>, home: Option<(&str, &Path)>) -> Option<String> {
    let command = command?;
    let Some((var, home)) = home else { return Some(command) };
    let home = home.display().to_string();
    let safe = home.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c));
    let home = if safe { home } else { format!("'{}'", home.replace('\'', r"'\''")) };
    Some(format!("{var}={home} {command}"))
}

impl Hub {
    /// `_acpmux/session/auth` for this daemon's chat `session`, or for `target` (a chat with
    /// no session yet, on this daemon). `refresh`: probe the route and run the harness's own
    /// check when nothing newer answers.
    pub(crate) async fn session_auth(
        self: &Arc<Self>,
        session: Option<&Arc<Session>>,
        target: Option<&SessionAuthTarget>,
        refresh: bool,
    ) -> Result<SessionAuthResult, RpcError> {
        // A chat whose log is not read yet (a deferred start) has only its summary.
        if let Some(session) = session {
            self.session_log(session).await;
        }
        let meta = session.map(|s| s.meta());
        let harness = match (&meta, target) {
            (Some(meta), _) => meta.harness.clone(),
            (None, Some(t)) => t.harness.clone(),
            (None, None) => {
                return Err(RpcError::invalid_params("sessionId or target is required"));
            }
        };
        let cfg = self.config.read().await;
        let profile = cfg.harnesses.get(&harness).cloned();
        if profile.is_none() && meta.is_none() {
            return Err(RpcError::invalid_params(format!("no harness named {harness:?}")));
        }
        let notes = cfg.profile_meta.get(&harness).and_then(|m| m.auth.clone());
        let (dir, state) = routes::places(&cfg);
        drop(cfg);
        let family = profile
            .as_ref()
            .map(|p| crate::config::derive_family(&harness, p))
            .unwrap_or_else(|| harness.clone());
        let bindings = routes::bindings(&state);
        let no_env = BTreeMap::new();
        let env = profile.as_ref().map_or(&no_env, |p| &p.env);
        // The scope that chose a target's route (`routes::spawn_route`).
        let mut scope = None;
        let route = match (&meta, target) {
            (Some(meta), _) => {
                let workspace = meta.session_env.get("CMUX_WORKSPACE_ID").map(String::as_str);
                routes::spawn_route(&bindings, &meta.id, workspace, &family, env)
                    .and_then(|(id, _)| routes::find(dir.as_deref(), &id).ok())
                    .filter(|r| r.serves(&family))
            }
            (None, Some(t)) => {
                let chosen = target_route(dir.as_deref(), &bindings, t, &family, env)?;
                scope = chosen.as_ref().map(|(_, scope)| *scope);
                chosen.map(|(route, _)| route)
            }
            (None, None) => None,
        };
        let route_id = route.as_ref().map(|r| r.id.clone());
        let home = match &meta {
            Some(meta) => self.home_of(meta, &family, route.as_ref()).await,
            None => target_home(profile.as_ref(), &family, route.as_ref().zip(scope)),
        };
        let var = route_account::home_var(&family);
        let mut facts = Facts {
            harness: harness.clone(),
            route_id: route_id.clone(),
            login: login_command(
                notes
                    .as_ref()
                    .and_then(|n| n.login.clone())
                    .or_else(|| crate::harness_health::builtin_login(&family).map(str::to_owned)),
                var.zip(home.as_deref()),
            ),
            ..Facts::default()
        };
        if let Some(id) = &route_id {
            // With refresh, a new probe (never of a route whose secret is in the keychain);
            // else, or for such a route, the last answer while it is fresh.
            let probe = route.as_ref().filter(|r| refresh && !routes::reads_keychain(r));
            facts.probe = match probe {
                Some(route) => Some((now_ms(), routes::probe(route, &state).await)),
                None => routes::recent_probe(id, PROBE_FRESH_MS),
            };
        }
        facts.turn = match (session, &meta) {
            (Some(s), Some(meta)) => {
                meta.last_turn.as_ref().and_then(turn_evidence).map(|t| (s.id.clone(), t))
            }
            _ => {
                let siblings =
                    Siblings { bindings: &bindings, family: &family, env, route: &route };
                self.sibling_turn(&harness, &siblings, &home).await
            }
        };
        if !facts.settled()
            && refresh
            && let Some(mut profile) = profile
        {
            // A chat's own env (its preset's and its chat store's over the profile's), as its
            // harness runs; a target's is the profile's.
            if let Some(meta) = &meta {
                profile.env = self.harness_env(meta).await.1;
            }
            if let (Some(var), Some(home)) = (var, &home) {
                profile.env.insert(var.to_owned(), home.display().to_string());
            }
            let id = harness.clone();
            let check = tokio::task::spawn_blocking(move || {
                crate::harness_health::fresh_auth(&id, &profile, notes.as_ref())
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            facts.check = Some((now_ms(), check));
        }
        Ok(facts.decide())
    }

    /// The account home `meta`'s chat runs in when it is not the CLI's default: its recorded
    /// store or its env's, else the route's for a chat that has not started.
    async fn home_of(
        &self,
        meta: &SessionMeta,
        family: &str,
        route: Option<&routes::Route>,
    ) -> Option<PathBuf> {
        let (_, env) = self.harness_env(meta).await;
        route_account::current_home(meta, family, &env).or_else(|| {
            let route_home = route.and_then(|r| r.file.account_home.as_ref());
            route_home.filter(|_| !route_account::has_home(meta)).map(PathBuf::from)
        })
    }

    /// The newest turn evidence of a chat here with this harness, route and account home.
    async fn sibling_turn(
        self: &Arc<Self>,
        harness: &str,
        like: &Siblings<'_>,
        home: &Option<PathBuf>,
    ) -> Option<(String, TurnEvidence)> {
        let dir = routes::places(&*self.config.read().await).0;
        // Whether a route id is there and serves the family, as a spawn checks it: once each.
        let mut serves = BTreeMap::<String, bool>::new();
        let want = like.route.as_ref().map(|r| r.id.as_str());
        let mut best: Option<(String, TurnEvidence)> = None;
        for s in self.sessions() {
            if s.meta().harness != harness {
                continue;
            }
            // A chat whose log is not read yet (a deferred start) has only its summary.
            self.session_log(&s).await;
            let meta = s.meta();
            let Some(turn) = meta.last_turn.as_ref().and_then(turn_evidence) else { continue };
            if meta.harness != harness || best.as_ref().is_some_and(|(_, b)| b.at >= turn.at) {
                continue;
            }
            let workspace = meta.session_env.get("CMUX_WORKSPACE_ID").map(String::as_str);
            let id = routes::spawn_route(like.bindings, &meta.id, workspace, like.family, like.env)
                .map(|(id, _)| id)
                .filter(|id| {
                    *serves.entry(id.clone()).or_insert_with(|| {
                        routes::find(dir.as_deref(), id).is_ok_and(|r| r.serves(like.family))
                    })
                });
            if id.as_deref() == want
                && self.home_of(&meta, like.family, like.route.as_ref()).await == *home
            {
                best = Some((s.id.clone(), turn));
            }
        }
        best
    }
}

/// What a target's sibling chats share with it, besides the harness.
struct Siblings<'a> {
    bindings: &'a routes::Bindings,
    family: &'a str,
    /// The harness profile's env, which may name its own route.
    env: &'a BTreeMap<String, String>,
    route: &'a Option<routes::Route>,
}

/// The route a new chat of `target` would spawn on, and the scope that chose it: its `routeId`
/// (as a chat's own), else the one a spawn picks (`routes::spawn_route`, with no chat or
/// workspace). An error for a route a spawn refuses: one that is gone, or that the target or
/// the profile names but does not serve the harness.
fn target_route(
    dir: Option<&Path>,
    bindings: &routes::Bindings,
    target: &SessionAuthTarget,
    family: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<(routes::Route, &'static str)>, RpcError> {
    let (id, scope) = match &target.route_id {
        Some(id) => (id.clone(), "chat"),
        None => {
            let mut bindings = bindings.clone();
            bindings.chats.clear();
            let Some(chosen) = routes::spawn_route(&bindings, "", None, family, env) else {
                return Ok(None);
            };
            chosen
        }
    };
    let route = routes::find(dir, &id)
        .map_err(|e| RpcError::invalid_params(format!("route {id}: {}", e.message())))?;
    if route.serves(family) {
        Ok(Some((route, scope)))
    } else if matches!(scope, "chat" | "profile") {
        Err(RpcError::invalid_params(format!("route {id} does not serve the {family} harness")))
    } else {
        Ok(None)
    }
}

/// The account home a new chat of `profile` would run in on `route` (and the scope that chose
/// it), as `Hub::apply_route_for` picks it: a home the profile pins is only replaced by a route
/// picked for the chat or the profile; else the route's home, else the profile's.
fn target_home(
    profile: Option<&HarnessProfile>,
    family: &str,
    route: Option<(&routes::Route, &str)>,
) -> Option<PathBuf> {
    let pinned = profile.and_then(|p| p.env.get(route_account::home_var(family)?)).map(|value| {
        let user = dirs::home_dir().unwrap_or_default();
        PathBuf::from(super::expand_env_value(value, &user, &user, ""))
    });
    let picked =
        route.filter(|(_, scope)| pinned.is_none() || matches!(*scope, "chat" | "profile"));
    picked.and_then(|(r, _)| r.file.account_home.as_ref().map(PathBuf::from)).or(pinned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(status: &str, kind: Option<&str>, text: &str, at: u64) -> Value {
        json!({"status": status, "errorKind": kind, "errorText": text, "endedAt": at})
    }

    fn facts(probe: Option<&str>, last: Option<Value>, check: Option<Result<Auth, &str>>) -> Facts {
        Facts {
            harness: "claude".into(),
            route_id: probe.map(|_| "proxy".into()),
            probe: probe.map(|s| (5, json!({"status": s, "message": format!("HTTP: {s}")}))),
            turn: last.as_ref().and_then(turn_evidence).map(|t| ("chat-1".into(), t)),
            check: check.map(|c| (9, c.map(|a| (a, "`check` ran".into())).map_err(str::to_owned))),
            login: Some("claude auth login".into()),
        }
    }

    /// Each state from the evidence that gives it, with its source and when it was seen.
    #[test]
    fn each_state_comes_from_its_evidence() {
        let signed_out = turn("failed", Some("auth.signed_out"), "Not logged in", 7);
        let expired = turn("failed", Some("auth.expired"), "OAuth token has expired", 7);
        for (f, state, source, at) in [
            (
                facts(None, Some(turn("completed", None, "", 7)), None),
                State::Ready,
                Source::Turn,
                Some(7),
            ),
            (facts(None, Some(signed_out), None), State::SignedOut, Source::Turn, Some(7)),
            (facts(None, Some(expired), None), State::Expired, Source::Turn, Some(7)),
            (
                facts(Some("auth_needed"), None, None),
                State::RouteAuthNeeded,
                Source::RouteProbe,
                Some(5),
            ),
            (
                facts(Some("unreachable"), None, None),
                State::RouteUnreachable,
                Source::RouteProbe,
                Some(5),
            ),
            (
                facts(None, None, Some(Ok(Auth::Expired))),
                State::Expired,
                Source::AuthCheck,
                Some(9),
            ),
            (facts(None, None, Some(Ok(Auth::SignedIn))), State::Ready, Source::AuthCheck, Some(9)),
            (
                facts(None, None, Some(Ok(Auth::SignedOut))),
                State::SignedOut,
                Source::AuthCheck,
                Some(9),
            ),
            (facts(None, None, None), State::Unknown, Source::None, None),
            (
                facts(None, None, Some(Ok(Auth::Unknown))),
                State::Unknown,
                Source::AuthCheck,
                Some(9),
            ),
        ] {
            let out = f.decide();
            assert_eq!((out.state, out.source, out.evidence_at), (state, source, at), "{out:?}");
            let blocks =
                matches!(state, State::SignedOut | State::Expired | State::RouteAuthNeeded);
            assert_eq!(out.login.is_some(), blocks, "{out:?}");
        }
    }

    /// No evidence, a check that could not answer, a connection error or a cancel: unknown,
    /// never signed out.
    #[test]
    fn no_evidence_and_transport_errors_are_unknown() {
        for last in [
            turn("failed", None, "API Error: Connection error.", 7),
            turn("failed", Some("agent.exited"), "agent process closed", 7),
            turn("cancelled", None, "", 7),
        ] {
            assert_eq!(turn_evidence(&last), None, "{last}");
            assert_eq!(facts(None, Some(last), None).decide().state, State::Unknown);
        }
        let out =
            facts(None, None, Some(Err("Claude Code may keep its login in its keychain"))).decide();
        assert_eq!((out.state, out.source, out.evidence_at), (State::Unknown, Source::None, None));
        assert!(out.detail.contains("keychain"), "{out:?}");
        // A turn recorded before the auth kinds is read by its text.
        let old = turn("failed", None, "Not logged in · Please run /login", 7);
        assert_eq!(turn_evidence(&old).map(|t| t.state), Some(State::SignedOut));
    }

    /// An agent's error of many lines and sentences is one sentence in `detail`.
    #[test]
    fn a_detail_is_one_sentence() {
        let text = "API Error: 401 OAuth token has expired. Please run /login\n\nat step 2";
        let out = facts(None, Some(turn("failed", Some("auth.expired"), text, 7)), None).decide();
        assert_eq!(out.detail, "its last turn failed: API Error: 401 OAuth token has expired.");
        assert_eq!(
            one_sentence("not logged in\nrun claude auth login"),
            "not logged in run claude auth login"
        );
        assert_eq!(one_sentence("see e.g. the docs"), "see e.g. the docs");
        assert_eq!(one_sentence("not logged in. run /login"), "not logged in.");
        assert_eq!(
            one_sentence("Claude said: \"OAuth token expired.\" Please run /login"),
            "Claude said: \"OAuth token expired.\""
        );
        assert_eq!(one_sentence("Error! try again"), "Error!");
        assert_eq!(one_sentence("use v2.1 i.e. the new one"), "use v2.1 i.e. the new one");
        let long = one_sentence(&"word ".repeat(100));
        assert!(long.chars().count() <= DETAIL_CHARS && long.ends_with('…'), "{long}");
    }

    /// A rate-limited or unknown probe says nothing of the sign-in: the turn answers.
    #[test]
    fn a_rate_limited_probe_falls_through() {
        for status in ["rate_limited", "unknown", "ok"] {
            let out = facts(Some(status), Some(turn("completed", None, "", 7)), None).decide();
            assert_eq!((out.state, out.source), (State::Ready, Source::Turn), "{status}");
            assert_eq!(out.route_id.as_deref(), Some("proxy"));
            let out = facts(Some(status), None, None).decide();
            assert_eq!(out.state, State::Unknown, "{status}");
        }
    }

    /// A target's account home is the one its spawn would use: a home the profile pins beats
    /// a family or global route's, not one picked for the chat or the profile.
    #[test]
    fn a_target_keeps_the_home_its_profile_pins() {
        let route = |home: &str| routes::Route {
            id: "r".into(),
            file: serde_json::from_value(
                json!({"kind": "direct-subscription", "account_home": home}),
            )
            .unwrap(),
            path: PathBuf::new(),
        };
        let pinned: HarnessProfile = serde_json::from_value(
            json!({"argv": ["codex"], "env": {"CODEX_HOME": "/accounts/A"}}),
        )
        .unwrap();
        let plain: HarnessProfile = serde_json::from_value(json!({"argv": ["codex"]})).unwrap();
        let b = route("/accounts/B");
        let home = |p: &HarnessProfile, scope| target_home(Some(p), "codex", Some((&b, scope)));
        assert_eq!(home(&pinned, "global"), Some(PathBuf::from("/accounts/A")));
        assert_eq!(home(&pinned, "family"), Some(PathBuf::from("/accounts/A")));
        assert_eq!(home(&pinned, "chat"), Some(PathBuf::from("/accounts/B")));
        assert_eq!(home(&plain, "global"), Some(PathBuf::from("/accounts/B")));
        assert_eq!(target_home(Some(&pinned), "codex", None), Some(PathBuf::from("/accounts/A")));
    }

    #[test]
    fn a_chat_in_its_own_account_home_signs_in_there() {
        let home = Path::new("/Users/me/claude two");
        assert_eq!(
            login_command(Some("claude auth login".into()), Some(("CLAUDE_CONFIG_DIR", home))),
            Some("CLAUDE_CONFIG_DIR='/Users/me/claude two' claude auth login".into())
        );
        assert_eq!(login_command(Some("codex login".into()), None), Some("codex login".into()));
    }
}
