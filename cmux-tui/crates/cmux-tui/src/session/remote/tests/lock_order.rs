//! The rank check (`crate::lock_rank`) on a remote session's and a mirror's
//! own locks: the nesting the session takes passes, an inversion panics in
//! debug and test builds and names both locks.

use super::*;

fn inversion_message(result: std::thread::Result<()>) -> String {
    let payload = result.expect_err("the inversion is detected");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_string()))
        .unwrap_or_default()
}

#[test]
fn the_session_nesting_passes_the_rank_check() {
    let (session, surface) = test_unleased_view_surface(7);
    {
        let _refresh = session.tree_refresh.lock().unwrap();
        let retired = session.retired_surfaces.lock().unwrap();
        let _tree = session.tree.lock().unwrap();
        drop(retired);
        let _pending = session.pending.lock().unwrap();
    }
    {
        let _lifecycle = session.cell_pixel_lifecycle.lock().unwrap();
        let surfaces = session.surfaces.lock().unwrap();
        let mirror = surfaces.get(&7).expect("the mirror is in the map");
        let _geometry = mirror.geometry_lifecycle.lock().unwrap();
        let _term = mirror.term.lock().unwrap();
        let _encoders = mirror.mouse_encoders.lock().unwrap();
        let _cell_pixels = mirror.cell_pixels.lock().unwrap();
    }
    // A stream resize and a cell metric change take the mirror's geometry,
    // terminal, encoders, provenance and cell size in order.
    surface.apply_stream_resize(20, 6, None, &[]).unwrap();
    assert!(surface.set_cell_pixel_size(9, 18).unwrap());
    session.retire_surface(7);
    assert!(session.surface_is_exited(7));
}

#[test]
fn a_mirror_lock_held_while_waiting_for_the_surface_map_panics() {
    let (session, surface) = test_unleased_view_surface(7);
    let term = surface.term.lock().unwrap();
    let message = inversion_message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(session.surfaces.lock());
    })));
    drop(term);
    assert!(
        message.contains("acquiring SessionSurfaces while this thread holds SurfaceTerm"),
        "{message}"
    );
}

/// `retire_surface` takes the retired set before the tree cache: called
/// with the tree cache held, it would wait in the opposite order of a tree
/// refresh.
#[test]
fn retiring_a_surface_under_the_tree_cache_panics() {
    let (session, _surface) = test_unleased_view_surface(7);
    let tree = session.tree.lock().unwrap();
    let message = inversion_message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.retire_surface(7);
    })));
    drop(tree);
    assert!(
        message.contains("acquiring SessionRetiredSurfaces while this thread holds SessionTree"),
        "{message}"
    );
}

/// Two mirrors' terminals have no order between them: two threads that
/// lock them in opposite orders deadlock.
#[test]
fn two_mirrors_terminals_held_at_once_panic() {
    let first = test_remote_surface(1);
    let second = test_remote_surface(2);
    let held = first.term.lock().unwrap();
    let message = inversion_message(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(second.term.lock());
    })));
    drop(held);
    assert!(
        message.contains("acquiring SurfaceTerm while this thread holds SurfaceTerm"),
        "{message}"
    );
}
