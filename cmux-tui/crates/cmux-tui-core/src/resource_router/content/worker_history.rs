//! `terminal.history.read` of a terminal a VT worker process parses: the
//! worker holds its history, so the rows are requested from it and the
//! result leaves from its answer (p01-process-135), never by waiting on the
//! request thread.

use std::sync::Arc;

use serde_json::Value;

use super::{
    HistoryReadFields, ParsedResourceRequest, history_read_value, resolve_terminal_surface,
    resource_operation_error,
};
use crate::Mux;
use crate::resource::ResourceError;
use crate::vt_worker_runtime::{HistoryRange, WorkerScreenSource, request_worker_history};

/// A checked `terminal.history.read` of a worker terminal, not sent yet.
pub(crate) struct WorkerHistoryRead {
    source: Arc<dyn WorkerScreenSource>,
    range: HistoryRange,
    styled: bool,
}

/// The read of `request` when a worker parses its terminal; `None` when the
/// daemon parses it (the synchronous read answers).
pub(crate) fn worker_history_read(
    mux: &Arc<Mux>,
    request: &ParsedResourceRequest,
) -> Result<Option<WorkerHistoryRead>, ResourceError> {
    let (_, surface) = resolve_terminal_surface(mux, &request.selectors)?;
    let Some(source) = mux.worker_screens().source(surface.id) else {
        return Ok(None);
    };
    let HistoryReadFields { before, limit, styled } = HistoryReadFields::of(request)?;
    // A `before` past u32 reads the newest page, as the daemon's read does.
    let before = before.and_then(|before| u32::try_from(before).ok());
    Ok(Some(WorkerHistoryRead { source, range: HistoryRange::Before { before, limit }, styled }))
}

impl WorkerHistoryRead {
    /// Ask the worker; `reply` gets the result once it answers.
    pub(crate) fn start(self, reply: impl FnOnce(Result<Value, ResourceError>) + Send + 'static) {
        let Self { source, range, styled } = self;
        request_worker_history(
            &source,
            range,
            Box::new(move |page| {
                reply(
                    page.map(|page| history_read_value(page.start, &page.rows, styled))
                        .map_err(|error| resource_operation_error(error.into())),
                );
            }),
        );
    }
}
