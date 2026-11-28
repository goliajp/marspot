//! Questions a service wants the active core to put to the person.
//!
//! A service registers where its questions come from; the event loop
//! asks the host for them when a core becomes the active one and on
//! every supervisor tick, and never names the service.

use std::sync::Mutex;

use marspot::shell_proto::InstallAsk;

use super::ShellPluginHost;

/// One source per service that asks; far more than there are.
const MAX: usize = 4;

/// The questions of one service: the ones the active core has not been
/// sent, or every unanswered one when `all`.
pub(crate) type Source = fn(all: bool) -> Vec<InstallAsk>;

static SOURCES: Mutex<Vec<Source>> = Mutex::new(Vec::new());

/// Called once per service, the first time it has a question.
pub(crate) fn register(source: Source) {
    let mut s = SOURCES.lock().unwrap_or_else(|p| p.into_inner());
    if s.len() < MAX {
        s.push(source);
    } else {
        marspot::lx_warn!("plugin.core_asks.full", "a question source was not registered: the table is full");
    }
}

impl ShellPluginHost {
    /// The questions to send the active core now, from every source.
    pub(crate) fn core_asks(&self, all: bool) -> Vec<InstallAsk> {
        // a source never registers while it answers, so holding the table is safe
        SOURCES.lock().unwrap_or_else(|p| p.into_inner()).iter().flat_map(|s| s(all)).collect()
    }
}
