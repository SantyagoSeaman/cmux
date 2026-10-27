//! The three decodes `connection_frame::dispatch_line` runs on a control
//! socket line (the head, the v2 envelope, the `Command`), with no mux,
//! for the cmux-tui/fuzz `control_request` target
//! (`crate::fuzz_entry::control_line`). Each call is the daemon's own.

use super::Command;
use super::connection_frame::command_request;
use super::request_head::RequestHead;
use crate::fuzz_entry::{ControlCommand, ControlLine};

pub(crate) fn control_line(message: &str) -> ControlLine {
    let head = RequestHead::parse(message);
    let command = command_request(&head, message)
        .map(|request| {
            let wire = request.cmd.wire_name();
            ControlCommand { wire, method: Command::method_of_wire(wire).map(|(id, _)| id) }
        })
        .map_err(|error| error.to_string());
    let resource = crate::resource_router::parse_resource_line(message).map(|envelope| {
        envelope.map(|envelope| envelope.operation.wire_name()).map_err(|error| error.message)
    });
    ControlLine {
        head_cmd: head.cmd().map(str::to_owned),
        head_operation: head.operation().map(str::to_owned),
        head_resource: head.is_resource(),
        head_command: head.is_command(),
        head_method: head.method().id,
        command,
        resource,
    }
}
