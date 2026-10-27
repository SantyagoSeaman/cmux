//! Where the daemon parses terminal output: in its own VT pool threads
//! (phase A, `vt_pool`) or in sandboxed `cmux-vt-worker` processes (phase B,
//! the broker of this module). terminal-host-model-DESIGN.md, option E;
//! plans/cmux-next/chrome-parity/p01-process.md, tasks 134 and 137.
//!
//! Behind the `vtWorkerProcesses` feature (`schemas/features/features.d`),
//! off on every channel until the phase B rows (135, 136, 141, 142 and
//! 209-211) pass; task 137 then turns its DEV default on in the registry, not
//! here. The mode resolves from the daemon's feature [`Snapshot`], the same
//! way `holder_runtime` resolves `terminalHolder`: the channel default, the
//! `CMUX_ENABLE_FEATURES` / `CMUX_DISABLE_FEATURES` lists, then managed
//! policy and the signed document's `DisabledFeatures`, which win over
//! everything. [`VT_WORKERS_ENV`] is one more env-list entry: `process`
//! enables, `threads` disables. This is Chromium's process-model rollout:
//! a `base::Feature` with a per-channel default and the
//! `--enable-features` / `--disable-features` switches
//! (`FeatureList::InitFromCommandLine`, base/feature_list.cc), where a
//! disable wins.

use cmux_features::{Snapshot, Source, feature::VT_WORKER_PROCESSES};

/// The env switch that predates `vtWorkerProcesses`: `process` enables the
/// feature and `threads` disables it.
pub const VT_WORKERS_ENV: &str = "CMUX_TUI_VT_WORKERS";

/// Where terminal output is parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VtParseMode {
    /// Phase A: the daemon's VT pool threads parse every terminal.
    Threads,
    /// Phase B: sandboxed `cmux-vt-worker` processes parse, sharded by
    /// trust class; the daemon reads their screen pages.
    Process,
}

impl VtParseMode {
    /// The name in [`VT_WORKERS_ENV`], `debug.vt` and the process census.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Threads => "threads",
            Self::Process => "process",
        }
    }

    fn from_enabled(enabled: bool) -> Self {
        if enabled { Self::Process } else { Self::Threads }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "threads" => Some(Self::Threads),
            "process" => Some(Self::Process),
            _ => None,
        }
    }
}

/// Why a daemon runs the mode it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VtParseModeSource {
    /// The `vtWorkerProcesses` registry default of the daemon's channel.
    ChannelDefault,
    /// The enable or disable list (spawn environment or client hello).
    FeatureList,
    /// Managed policy or the signed document's `DisabledFeatures`.
    KillSwitch,
    /// A valid [`VT_WORKERS_ENV`] value.
    Alias,
    /// [`VT_WORKERS_ENV`] held a value that names no mode; the feature's
    /// value applies. The value is kept for the startup log line.
    InvalidAlias(String),
}

/// The parse mode of one daemon, decided once at start. The broker and the
/// VT pool read it; it never changes while the daemon runs (the feature is
/// not `runtimeMutable`), so a terminal never moves between modes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VtWorkerPolicy {
    mode: VtParseMode,
    source: VtParseModeSource,
}

impl VtWorkerPolicy {
    /// The policy from the daemon's resolved `features` and its
    /// [`VT_WORKERS_ENV`] value `alias`. A disable on the feature lists wins
    /// over the alias's `process`, `DisabledFeatures` wins over both, and the
    /// alias's `threads` wins over an enable. Blank counts as unset.
    pub fn resolve(features: &Snapshot, alias: Option<&str>) -> Self {
        let feature = VtParseMode::from_enabled(features.is_enabled(VT_WORKER_PROCESSES));
        let alias = alias.filter(|value| !value.trim().is_empty());
        let (mode, source) = match (features.source(VT_WORKER_PROCESSES), alias) {
            (Source::Disabled, _) => (VtParseMode::Threads, VtParseModeSource::KillSwitch),
            (Source::Override, _) if feature == VtParseMode::Threads => {
                (VtParseMode::Threads, VtParseModeSource::FeatureList)
            }
            (_, Some(value)) => match VtParseMode::parse(value) {
                Some(mode) => (mode, VtParseModeSource::Alias),
                None => (feature, VtParseModeSource::InvalidAlias(value.to_owned())),
            },
            (Source::Override, None) => (feature, VtParseModeSource::FeatureList),
            (Source::Default, None) => (feature, VtParseModeSource::ChannelDefault),
        };
        Self { mode, source }
    }

    /// The policy of this daemon process: its resolved `features` and the
    /// alias from its environment.
    pub fn for_daemon(features: &Snapshot) -> Self {
        Self::resolve(features, std::env::var(VT_WORKERS_ENV).ok().as_deref())
    }

    pub fn mode(&self) -> VtParseMode {
        self.mode
    }

    pub fn source(&self) -> &VtParseModeSource {
        &self.source
    }

    /// Whether this daemon starts `cmux-vt-worker` processes.
    pub fn workers_enabled(&self) -> bool {
        self.mode == VtParseMode::Process
    }

    /// One line for the startup log and `debug.vt`, e.g.
    /// `vt parse: threads (vtWorkerProcesses channel default)`. An invalid
    /// alias names its value; the value is a mode name the person typed, not
    /// terminal data.
    pub fn describe(&self) -> String {
        const FEATURE: &str = "vtWorkerProcesses";
        let mode = self.mode.name();
        match &self.source {
            VtParseModeSource::ChannelDefault => {
                format!("vt parse: {mode} ({FEATURE} channel default)")
            }
            VtParseModeSource::FeatureList => format!("vt parse: {mode} ({FEATURE} feature list)"),
            VtParseModeSource::KillSwitch => {
                format!("vt parse: {mode} ({FEATURE} disabled by policy)")
            }
            VtParseModeSource::Alias => format!("vt parse: {mode} ({VT_WORKERS_ENV})"),
            VtParseModeSource::InvalidAlias(value) => format!(
                "vt parse: {mode} ({FEATURE}; ignored {VT_WORKERS_ENV}={value:?}, expected threads or process)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmux_features::Channel;

    const CHANNELS: [Channel; 3] = [Channel::Dev, Channel::Nightly, Channel::Release];
    const NAME: &str = "vtWorkerProcesses";

    fn lists(channel: Channel, enable: Option<&str>, disable: Option<&str>) -> Snapshot {
        Snapshot::from_env(channel, enable, disable)
    }

    #[test]
    fn every_channel_defaults_to_pool_threads() {
        for channel in CHANNELS {
            for features in [Snapshot::defaults(channel), lists(channel, None, None)] {
                let policy = VtWorkerPolicy::resolve(&features, None);
                assert_eq!(policy.mode(), VtParseMode::Threads, "{channel:?}");
                assert_eq!(policy.source(), &VtParseModeSource::ChannelDefault, "{channel:?}");
                assert!(!policy.workers_enabled(), "{channel:?}");
            }
        }
    }

    #[test]
    fn the_feature_lists_turn_workers_on_and_off_on_every_channel() {
        for channel in CHANNELS {
            let on = VtWorkerPolicy::resolve(&lists(channel, Some(NAME), None), None);
            assert_eq!(on.mode(), VtParseMode::Process, "{channel:?}");
            assert_eq!(on.source(), &VtParseModeSource::FeatureList);
            let both = VtWorkerPolicy::resolve(&lists(channel, Some(NAME), Some(NAME)), None);
            assert_eq!(both.mode(), VtParseMode::Threads, "{channel:?}: disable wins");
            let hello = Snapshot::from_pairs(channel, [(NAME, true)]);
            assert!(VtWorkerPolicy::resolve(&hello, None).workers_enabled(), "{channel:?}");
        }
    }

    #[test]
    fn the_env_alias_selects_the_mode_on_every_channel() {
        for channel in CHANNELS {
            let defaults = Snapshot::defaults(channel);
            let process = VtWorkerPolicy::resolve(&defaults, Some(" process\n"));
            assert_eq!(process.mode(), VtParseMode::Process, "{channel:?}");
            assert_eq!(process.source(), &VtParseModeSource::Alias);
            let on = lists(channel, Some(NAME), None);
            let threads = VtWorkerPolicy::resolve(&on, Some("threads"));
            assert_eq!(threads.mode(), VtParseMode::Threads, "{channel:?}: alias disable wins");
            assert_eq!(threads.source(), &VtParseModeSource::Alias);
        }
    }

    #[test]
    fn a_disable_beats_the_alias() {
        for channel in CHANNELS {
            let off = lists(channel, None, Some(NAME));
            let policy = VtWorkerPolicy::resolve(&off, Some("process"));
            assert_eq!(policy.mode(), VtParseMode::Threads, "{channel:?}");
            assert_eq!(policy.source(), &VtParseModeSource::FeatureList);
        }
    }

    #[test]
    fn disabled_features_beat_every_other_input() {
        for channel in CHANNELS {
            let killed = lists(channel, Some(NAME), None).with_disabled([NAME]);
            let policy = VtWorkerPolicy::resolve(&killed, Some("process"));
            assert_eq!(policy.mode(), VtParseMode::Threads, "{channel:?}");
            assert_eq!(policy.source(), &VtParseModeSource::KillSwitch);
            assert_eq!(
                policy.describe(),
                "vt parse: threads (vtWorkerProcesses disabled by policy)"
            );
        }
    }

    #[test]
    fn blank_alias_is_unset() {
        for channel in CHANNELS {
            for features in [Snapshot::defaults(channel), lists(channel, Some(NAME), None)] {
                assert_eq!(
                    VtWorkerPolicy::resolve(&features, Some("  ")),
                    VtWorkerPolicy::resolve(&features, None),
                    "{channel:?}"
                );
            }
        }
    }

    #[test]
    fn invalid_alias_keeps_the_feature_value_and_says_so() {
        let release = VtWorkerPolicy::resolve(&Snapshot::defaults(Channel::Release), Some("on"));
        assert_eq!(release.mode(), VtParseMode::Threads);
        assert_eq!(release.source(), &VtParseModeSource::InvalidAlias("on".into()));
        assert!(release.describe().contains("ignored CMUX_TUI_VT_WORKERS=\"on\""));
        let enabled = lists(Channel::Dev, Some(NAME), None);
        let dev = VtWorkerPolicy::resolve(&enabled, Some("PROCESS"));
        assert_eq!(dev.mode(), VtParseMode::Process, "values are case-sensitive");
        assert!(matches!(dev.source(), VtParseModeSource::InvalidAlias(_)));
    }

    #[test]
    fn mode_names_round_trip_through_the_alias() {
        for mode in [VtParseMode::Threads, VtParseMode::Process] {
            assert_eq!(VtParseMode::parse(mode.name()), Some(mode));
        }
    }

    #[test]
    fn describe_names_mode_and_source() {
        assert_eq!(
            VtWorkerPolicy::resolve(&Snapshot::defaults(Channel::Dev), None).describe(),
            "vt parse: threads (vtWorkerProcesses channel default)"
        );
        assert_eq!(
            VtWorkerPolicy::resolve(&Snapshot::defaults(Channel::Nightly), Some("process"))
                .describe(),
            "vt parse: process (CMUX_TUI_VT_WORKERS)"
        );
    }
}
