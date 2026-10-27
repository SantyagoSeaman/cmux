//! Lock ranks (lockdep style) for the TUI client's long-lived locks: this
//! crate's rank family for the shared `cmux_lock_rank` crate, which holds
//! [`RankedMutex`], the per-thread rank stack and the order check
//! (Chromium's base/task/common/checked_lock.h).
//!
//! The family covers the app's `OrderedSession` (the attach executor,
//! ambiguous creations and the surface attach, resize and sidebar plugin
//! sync maps), the remote session (`RemoteSession`, its `RemoteSidebarDoc`
//! and each mirrored `RemoteSurface`) and the machine provider hub
//! (`ProviderClientInner`). The UI thread, the ordered session worker, the
//! attach workers, the session reader and the provider reader take them at
//! once. The order, outer first, is [`LockRank`]'s; each rank is one lock
//! (one per mirror for the surface ranks), except [`LockRank::Leaf`].
//!
//! A blocking acquisition at a rank equal to or before a held rank panics in
//! debug and test builds and names both locks; `try_lock` is recorded, not
//! checked. Release builds compile the rank, the name and the stack out.
//! Debug builds time a contended wait and record a `lock-wait` site at the
//! caller (`cmux_threads::lock_wait::note_timed_lock_wait`), as
//! `cmux_threads::sync::Mutex` does.
//!
//! Locks a thread waits on with a `Condvar` (the interactive write queue,
//! the attach executor's job queue, a provider event queue) stay unranked:
//! the shared crate has no ranked condition variable.

use std::panic::Location;
use std::time::Duration;

/// A `Mutex` with a lock rank. The API mirrors `std::sync::Mutex`.
pub(crate) type RankedMutex<T> = cmux_lock_rank::RankedMutex<LockRank, T>;
/// A held [`RankedMutex`]. The lock is released before its rank.
pub(crate) type RankedGuard<'a, T> = cmux_lock_rank::RankedGuard<'a, LockRank, T>;

/// Rank of a ranked lock. A thread acquires ranks in ascending order.
///
/// The app ranks come first: the app calls into the session with some of
/// them held, and the session never calls back with its own held. The
/// remote session order comes from the nesting the suite takes (a
/// recording run over all of `nextest -p cmux-tui`) and a scan of every
/// guard held across another lock. The provider hub locks take no other
/// ranked lock while held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LockRank {
    /// `OrderedSession::remote_surface_attaches`: a shutdown drops queued
    /// attach jobs under it, and each job releases its attach claim.
    AppSurfaceAttachExecutor,
    /// `OrderedSession::ambiguous_creations`.
    AppAmbiguousCreations,
    /// `OrderedSession::retired_surfaces`: a retirement and an attach
    /// preflight read the attach claims under it.
    AppRetiredSurfaces,
    /// `OrderedSession::surface_attach_claims`: the ordered attach records
    /// its failure under it.
    AppSurfaceAttachClaims,
    /// `OrderedSession::surface_attach_failures`.
    AppSurfaceAttachFailures,
    /// `OrderedSession::surface_resize_ownership`.
    AppSurfaceResizeOwnership,
    /// `OrderedSession::surface_resize_failures`.
    AppSurfaceResizeFailures,
    /// `OrderedSession::surface_resize_claims`.
    AppSurfaceResizeClaims,
    /// `OrderedSession::sidebar_plugin_sync`.
    AppSidebarPluginSync,
    /// `RemoteSession::tree_refresh`, held for a whole tree refresh: its
    /// requests (`pending`), the retired set, the tree cache and the
    /// browser sources.
    SessionTreeRefresh,
    /// `RemoteSession::cell_pixel_lifecycle`: a cell metric change and an
    /// attach commit or retirement take the surface map, the recovery and
    /// lease maps and each mirror's geometry under it.
    SessionCellPixelLifecycle,
    /// `RemoteSession::frame_logs`: the frame dump on drop walks the
    /// surfaces and their terminals under it.
    SessionFrameLogs,
    /// `RemoteSession::retired_surfaces`: the tree cache drops retired
    /// agents under it.
    SessionRetiredSurfaces,
    /// `RemoteSession::exited_surfaces`: a retirement clears the surface's
    /// lease and overflow recovery under it.
    SessionExitedSurfaces,
    /// `RemoteSession::browser_sources`: an attach falls back to the tree
    /// cache under it.
    SessionBrowserSources,
    /// `RemoteSession::surfaces`: a walk of the map reads each mirror's
    /// locks.
    SessionSurfaces,
    /// `RemoteSession::tree`.
    SessionTree,
    /// `RemoteSession::surface_leases`.
    SessionSurfaceLeases,
    /// `RemoteSession::surface_overflow_recovery`.
    SessionSurfaceOverflowRecovery,
    /// `RemoteSession::subscription_recovery`: events are emitted under it.
    SessionSubscriptionRecovery,
    /// `RemoteSession::primed_subscription`.
    SessionPrimedSubscription,
    /// `RemoteSession::pending`, the requests waiting for a response.
    SessionPending,
    /// `RemoteSession::capabilities`.
    SessionCapabilities,
    /// `RemoteSession::size_states`.
    SessionSizeStates,
    /// `RemoteSession::cell_pixels`.
    SessionCellPixels,
    /// `RemoteSidebarDoc::stream`: a check that the stream is open reads
    /// the model in the same condition.
    SidebarStream,
    /// `RemoteSidebarDoc::model`.
    SidebarModel,
    /// `RemoteSidebarDoc::cursor`.
    SidebarCursor,
    /// `RemoteSidebarDoc::pending`.
    SidebarPending,
    /// `RemoteSidebarDoc::fallback_read_at`.
    SidebarFallbackReadAt,
    /// `RemoteSurface::geometry_lifecycle`, held for a stream resize or a
    /// cell metric change: the terminal, encoders and cell size under it.
    SurfaceGeometryLifecycle,
    /// `RemoteSurface::term`: a resize or a terminal callback updates the
    /// mouse encoders, the cursor provenance and the cell size under it.
    SurfaceTerm,
    /// `RemoteSurface::mouse_encoders`.
    SurfaceMouseEncoders,
    /// `RemoteSurface::cursor_provenance`.
    SurfaceCursorProvenance,
    /// `RemoteSurface::cell_pixels`.
    SurfaceCellPixels,
    /// `RemoteSurface::reported_size`.
    SurfaceReportedSize,
    /// `RemoteSurface::browser`.
    SurfaceBrowser,
    /// `ProviderClientInner::events`: publishing wakes each subscriber's
    /// (unranked) event queue under it.
    ProviderEvents,
    /// `ProviderClientInner::snapshot_subscribers`.
    ProviderSnapshotSubscribers,
    /// `ProviderClientInner::pending`, the requests waiting for a response.
    ProviderPending,
    /// `ProviderClientInner::writer`, held for one control frame write.
    ProviderWriter,
    /// `ProviderClientInner::token`.
    ProviderToken,
    /// `ProviderClientInner::provider_capabilities`.
    ProviderCapabilities,
    /// `ProviderClientInner::negotiated_client_capabilities`.
    ProviderClientCapabilities,
    /// Locks that take no ranked lock while held (the test hooks and
    /// markers); release builds pass it to the family's hooks.
    Leaf,
}

impl cmux_lock_rank::Rank for LockRank {
    const ALL: &'static [Self] = &[
        Self::AppSurfaceAttachExecutor,
        Self::AppAmbiguousCreations,
        Self::AppRetiredSurfaces,
        Self::AppSurfaceAttachClaims,
        Self::AppSurfaceAttachFailures,
        Self::AppSurfaceResizeOwnership,
        Self::AppSurfaceResizeFailures,
        Self::AppSurfaceResizeClaims,
        Self::AppSidebarPluginSync,
        Self::SessionTreeRefresh,
        Self::SessionCellPixelLifecycle,
        Self::SessionFrameLogs,
        Self::SessionRetiredSurfaces,
        Self::SessionExitedSurfaces,
        Self::SessionBrowserSources,
        Self::SessionSurfaces,
        Self::SessionTree,
        Self::SessionSurfaceLeases,
        Self::SessionSurfaceOverflowRecovery,
        Self::SessionSubscriptionRecovery,
        Self::SessionPrimedSubscription,
        Self::SessionPending,
        Self::SessionCapabilities,
        Self::SessionSizeStates,
        Self::SessionCellPixels,
        Self::SidebarStream,
        Self::SidebarModel,
        Self::SidebarCursor,
        Self::SidebarPending,
        Self::SidebarFallbackReadAt,
        Self::SurfaceGeometryLifecycle,
        Self::SurfaceTerm,
        Self::SurfaceMouseEncoders,
        Self::SurfaceCursorProvenance,
        Self::SurfaceCellPixels,
        Self::SurfaceReportedSize,
        Self::SurfaceBrowser,
        Self::ProviderEvents,
        Self::ProviderSnapshotSubscribers,
        Self::ProviderPending,
        Self::ProviderWriter,
        Self::ProviderToken,
        Self::ProviderCapabilities,
        Self::ProviderClientCapabilities,
        Self::Leaf,
    ];
    const ORDER: &'static str = "cmux_tui::lock_rank::LockRank's: ordered session (attach \
         executor > ambiguous creations > retired > attach claims > attach failures > resize \
         ownership > resize failures > resize claims > sidebar plugin sync) > remote session \
         > sidebar doc > remote surface > machine provider hub > leaf";
    const UNRANKED: Self = Self::Leaf;
    type Wait = ();
    type Hold = ();

    #[inline]
    fn ordinal(self) -> usize {
        self as usize
    }

    #[inline]
    fn before_wait(self) {}

    #[inline]
    fn on_held(self, (): ()) {}

    #[inline]
    fn note_contended_wait(waited: Duration, site: &'static Location<'static>) {
        cmux_threads::lock_wait::note_timed_lock_wait(waited, site);
    }
}

#[cfg(test)]
mod tests {
    use super::LockRank;
    use cmux_lock_rank::Rank;

    #[test]
    fn every_rank_is_listed_in_order() {
        for (ordinal, rank) in LockRank::ALL.iter().enumerate() {
            assert_eq!(rank.ordinal(), ordinal, "{rank:?}");
        }
        assert_eq!(LockRank::ALL.last(), Some(&LockRank::Leaf));
        assert!(LockRank::ALL.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
