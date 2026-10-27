//! One control socket line through the decodes the daemon runs on it
//! (server/connection_frame.rs `dispatch_line`): the request head
//! (server/request_head.rs), the v2 envelope parse
//! (resource_router.rs `parse_resource_line`) and the `Command` parse. The
//! head routes the line, so it must agree with the parse that answers it,
//! as a Mojo message's header names the one receiver that decodes it
//! (mojo/public/cpp/bindings/lib/multiplex_router.cc, `Accept`).
//!
//! Properties, beyond "no crash, leak or hang":
//! - a line the `Command` parse accepts goes down the `Command` path, and
//!   its span names the command that runs (head method = command method);
//! - a line the envelope parse takes as v2 has a head with `protocol`, and
//!   an accepted envelope's operation is the head's `operation`;
//! - decoding is a function of the line: a second decode is equal.

#![no_main]

use cmux_tui_core::fuzz_entry::control_line;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The reader hands the dispatcher UTF-8 lines only.
    let Ok(line) = std::str::from_utf8(data) else { return };
    let decoded = control_line(line);

    if let Ok(command) = &decoded.command {
        assert!(decoded.head_command, "a `{}` line skips the Command path: {line}", command.wire);
        assert!(command.method.is_some(), "the method table has no `{}`", command.wire);
        assert_eq!(
            Some(decoded.head_method),
            command.method,
            "the span names another method than `{}` runs: {line}",
            command.wire
        );
    }
    if let Some(resource) = &decoded.resource {
        assert!(decoded.head_resource, "a v2 line's head has no `protocol`: {line}");
        if let Ok(operation) = resource {
            assert_eq!(
                decoded.head_operation.as_deref(),
                Some(*operation),
                "the head routes another operation than the envelope names: {line}"
            );
        }
    }
    assert_eq!(control_line(line), decoded, "two decodes of one line differ: {line}");
});
