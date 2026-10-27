//! The rank check (`crate::lock_rank`) on the ordered session's own locks:
//! the nesting a retirement and an attach take passes, an inversion panics
//! in debug and test builds and names both locks.

use super::*;

fn inversion_message(result: std::thread::Result<()>) -> String {
    let payload = result.expect_err("the inversion is detected");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_string()))
        .unwrap_or_default()
}

fn remote_app() -> App {
    let mux = Mux::new("ordered-session-lock-order-test", SurfaceOptions::default());
    let mut app = test_app(Session::Local(mux));
    app.session.remote = true;
    app
}

/// `forget_surface` takes the retired set, then the attach claims, then the
/// failure and ownership maps; the attach preflight takes the retired set
/// and then the claims.
#[test]
fn the_ordered_session_nesting_passes_the_rank_check() {
    let app = remote_app();
    let session = &app.session;
    session.surface_attach_claims.lock().unwrap().insert(7, SurfaceAttachClaimState::default());
    session.forget_surface(7);
    assert!(session.retired_surfaces.lock().unwrap().contains(&7));
    assert!(session.surface_attach_claims.lock().unwrap().get(&7).unwrap().retired);
    {
        let _executor = session.remote_surface_attaches.lock().unwrap();
        let _retired = session.retired_surfaces.lock().unwrap();
        let _claims = session.surface_attach_claims.lock().unwrap();
        let _failures = session.surface_attach_failures.lock().unwrap();
    }
    assert!(!session.can_attach_surface(7));
}

/// An attach claim held while waiting for the retired set is the opposite
/// order of `forget_surface` on the UI thread and the attach worker's
/// preflight: the two deadlock.
#[test]
fn the_retired_set_taken_under_an_attach_claim_panics() {
    let app = remote_app();
    let session = &app.session;
    let claims = session.surface_attach_claims.lock().unwrap();
    let message = inversion_message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(session.retired_surfaces.lock());
    })));
    drop(claims);
    assert!(
        message.contains(
            "acquiring ordered_session.retired_surfaces (AppRetiredSurfaces) while this thread \
             holds ordered_session.surface_attach_claims (AppSurfaceAttachClaims)"
        ),
        "{message}"
    );
}

/// `forget_surface` called with an attach claim held waits for the retired
/// set inside it: the check fires at that acquisition.
#[test]
fn forgetting_a_surface_under_an_attach_claim_panics() {
    let app = remote_app();
    let session = &app.session;
    let claims = session.surface_attach_claims.lock().unwrap();
    let message = inversion_message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.forget_surface(7);
    })));
    drop(claims);
    assert!(message.contains("acquiring ordered_session.retired_surfaces"), "{message}");
}
