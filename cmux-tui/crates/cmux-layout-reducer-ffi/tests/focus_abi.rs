//! The `cmux_focus_*` C ABI as the app calls it: a handle, borrowed
//! topology and event arrays, replies and string lists that point into the
//! handle, and an exported JSON string the caller frees. focus_rules.rs checks
//! the rules; this file checks the pointers, so it is small enough for Miri
//! (scripts/cmux-next/linux/miri.sh).

use cmux_layout_reducer_ffi::focus::{
    CmuxFocusColumn, CmuxFocusEvent, CmuxFocusPane, CmuxFocusReply, CmuxFocusStr, CmuxFocusStrings,
    CmuxFocusTab, CmuxFocusTopology, cmux_focus_changes_view, cmux_focus_current,
    cmux_focus_export_json, cmux_focus_free, cmux_focus_import_json, cmux_focus_new,
    cmux_focus_recent_panes, cmux_focus_remembered, cmux_focus_send, cmux_focus_set_close_focus,
    cmux_focus_set_topology, cmux_focus_string_free,
};
use std::ffi::CStr;

/// The caller's string table: every name the topology and events use.
struct Table(Vec<u8>);

impl Table {
    fn new() -> Self {
        Table(Vec::new())
    }

    fn add(&mut self, text: &str) -> CmuxFocusStr {
        let offset = self.0.len() as u32;
        self.0.extend_from_slice(text.as_bytes());
        CmuxFocusStr { offset, len: text.len() as u32 }
    }
}

fn event(kind: u8, a: CmuxFocusStr, c: CmuxFocusStr) -> CmuxFocusEvent {
    CmuxFocusEvent {
        kind,
        source: 1, // keyboard
        target: 0,
        target_keyboard: 0,
        value: 0,
        reserved: [0; 3],
        list_count: 0,
        reserved2: 0,
        generation: 0,
        a,
        b: CmuxFocusStr::ABSENT,
        c,
        list: std::ptr::null(),
    }
}

/// The text a reply or list string names, read through its raw pointer.
fn text(bytes: *const u8, len: u32, s: CmuxFocusStr) -> Option<String> {
    if s.offset == u32::MAX {
        return None;
    }
    assert!(s.offset as usize + s.len as usize <= len as usize, "a string past its buffer");
    // SAFETY: the ABI's contract: `bytes` holds `len` bytes until the next call.
    let all = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
    let range = s.offset as usize..(s.offset + s.len) as usize;
    Some(String::from_utf8(all[range].to_vec()).expect("utf-8"))
}

fn reply_pane(reply: *const CmuxFocusReply) -> Option<String> {
    assert!(!reply.is_null());
    // SAFETY: a reply stays valid until the next call on the handle.
    let reply = unsafe { &*reply };
    assert_eq!(reply.ok, 1);
    text(reply.strings, reply.strings_len, reply.pane)
}

fn list(strings: *const CmuxFocusStrings) -> Vec<String> {
    assert!(!strings.is_null());
    // SAFETY: a list stays valid until the next call on the handle.
    let strings = unsafe { &*strings };
    if strings.count == 0 {
        return Vec::new();
    }
    // SAFETY: `items` holds `count` entries.
    let items = unsafe { std::slice::from_raw_parts(strings.items, strings.count as usize) };
    items.iter().map(|s| text(strings.bytes, strings.bytes_len, *s).expect("item")).collect()
}

/// One workspace `w`: one column with pane `p1` (tabs `t1`, `t2`) and pane `p2` (tab `t3`).
fn set_topology(focus: *mut cmux_layout_reducer_ffi::focus::Focus) -> *const CmuxFocusReply {
    let mut table = Table::new();
    let workspace = table.add("w");
    let (p1, p2) = (table.add("p1"), table.add("p2"));
    let (t1, t2, t3) = (table.add("t1"), table.add("t2"), table.add("t3"));
    let tab = |id| CmuxFocusTab { id, surface: CmuxFocusStr::ABSENT, kind: 0, reserved: 0 };
    let tabs = [tab(t1), tab(t2), tab(t3)];
    let panes = [
        CmuxFocusPane { id: p1, selected: t1, tab_start: 0, tab_count: 2 },
        CmuxFocusPane { id: p2, selected: t3, tab_start: 2, tab_count: 1 },
    ];
    let column = table.add("c1");
    let columns =
        [CmuxFocusColumn { id: column, screen: 0, pane_start: 0, pane_count: 2, reserved: 0 }];
    let column_panes = [p1, p2];
    let topology = CmuxFocusTopology {
        workspace,
        panes: panes.as_ptr(),
        tabs: tabs.as_ptr(),
        columns: columns.as_ptr(),
        column_panes: column_panes.as_ptr(),
        pane_count: panes.len() as u32,
        tab_count: tabs.len() as u32,
        column_count: columns.len() as u32,
        column_pane_count: column_panes.len() as u32,
        screen_count: 1,
        reserved: 0,
    };
    // SAFETY: a live handle; every array holds its count; the table outlives the call.
    unsafe { cmux_focus_set_topology(focus, &topology, table.0.as_ptr(), table.0.len()) }
}

fn focus_pane(
    focus: *mut cmux_layout_reducer_ffi::focus::Focus,
    pane: &str,
) -> *const CmuxFocusReply {
    let mut table = Table::new();
    let (pane, workspace) = (table.add(pane), table.add("w"));
    let event = event(0, pane, workspace);
    // SAFETY: a live handle, a readable event and table.
    unsafe { cmux_focus_send(focus, &event, table.0.as_ptr(), table.0.len()) }
}

#[test]
fn a_handle_reduces_borrowed_input_and_replies_through_its_own_buffers() {
    let focus = cmux_focus_new();
    assert!(!set_topology(focus).is_null());
    assert_eq!(reply_pane(focus_pane(focus, "p2")).as_deref(), Some("p2"));
    assert_eq!(reply_pane(focus_pane(focus, "p1")).as_deref(), Some("p1"));
    // SAFETY: a live handle, used by this thread only.
    unsafe {
        assert_eq!(reply_pane(cmux_focus_current(focus)).as_deref(), Some("p1"));
        cmux_focus_set_close_focus(focus, 1);
        let recent = list(cmux_focus_recent_panes(focus, std::ptr::null(), 0, 0));
        assert_eq!(recent.first().map(String::as_str), Some("p1"), "{recent:?}");
        assert!(recent.iter().any(|pane| pane == "p2"), "{recent:?}");
        let named = list(cmux_focus_recent_panes(focus, b"w".as_ptr(), 1, 1));
        assert_eq!(named, recent);
        assert_eq!(list(cmux_focus_remembered(focus)), ["w", "p1"]);
        cmux_focus_free(focus);
    }
}

#[test]
fn an_exported_state_imports_into_a_new_handle() {
    let focus = cmux_focus_new();
    set_topology(focus);
    focus_pane(focus, "p2");
    // SAFETY: live handles; the exported string is freed once with its own free.
    unsafe {
        let exported = cmux_focus_export_json(focus);
        assert!(!exported.is_null());
        let json = CStr::from_ptr(exported).to_bytes().to_vec();
        cmux_focus_string_free(exported);
        let copy = cmux_focus_new();
        assert_eq!(cmux_focus_import_json(copy, json.as_ptr(), json.len()), 0);
        assert_eq!(reply_pane(cmux_focus_current(copy)).as_deref(), Some("p2"));
        // A bad import keeps the state.
        assert_eq!(cmux_focus_import_json(copy, b"{".as_ptr(), 1), -2);
        assert_eq!(reply_pane(cmux_focus_current(copy)).as_deref(), Some("p2"));
        cmux_focus_free(copy);
        cmux_focus_free(focus);
    }
}

#[test]
fn bad_input_is_refused_without_touching_the_handle() {
    let focus = cmux_focus_new();
    set_topology(focus);
    focus_pane(focus, "p1");
    let mut table = Table::new();
    let pane = table.add("p2");
    let past_the_end = CmuxFocusStr { offset: 100, len: 4 };
    let unknown_kind = event(99, pane, CmuxFocusStr::ABSENT);
    let bad_string = event(0, past_the_end, CmuxFocusStr::ABSENT);
    // SAFETY: a live handle; readable events and table.
    unsafe {
        for bad in [&unknown_kind, &bad_string] {
            let reply = cmux_focus_send(focus, bad, table.0.as_ptr(), table.0.len());
            assert!(!reply.is_null());
            assert_eq!((*reply).ok, 0);
            assert_eq!(cmux_focus_changes_view(bad, table.0.as_ptr(), table.0.len()), 0);
        }
        // A null strings pointer with a length is refused, not read.
        let good = event(0, pane, CmuxFocusStr::ABSENT);
        assert_eq!((*cmux_focus_send(focus, &good, std::ptr::null(), 3)).ok, 0);
        assert!(cmux_focus_send(std::ptr::null_mut(), &good, table.0.as_ptr(), 2).is_null());
        assert!(cmux_focus_current(std::ptr::null_mut()).is_null());
        assert_eq!(reply_pane(cmux_focus_current(focus)).as_deref(), Some("p1"));
        cmux_focus_free(focus);
        cmux_focus_free(std::ptr::null_mut());
        cmux_focus_string_free(std::ptr::null_mut());
    }
}
