//! The far end of a [`super::StepKind::AwaitJob`].

use std::sync::{Arc, Mutex};

/// Whoever does the work calls [`finish`](Self::finish) once, from any
/// thread, optionally with text for a later [`super::StepKind::PasteJob`]
/// to deliver.
#[derive(Default)]
pub struct Job {
    result: Mutex<Option<Result<Option<String>, String>>>,
    /// Said beside the badge while a step waits on this job.
    note: Option<String>,
}

impl Job {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    /// A job whose wait the badge names, for one that can take a while.
    pub fn noted(note: impl Into<String>) -> Arc<Self> {
        Arc::new(Self { note: Some(note.into()), ..Self::default() })
    }
    /// Report the outcome.  Only the first report counts.
    pub fn finish(&self, r: Result<Option<String>, String>) {
        let mut slot = self.result.lock().unwrap();
        if slot.is_none() {
            *slot = Some(r);
        }
    }
    pub(crate) fn poll(&self) -> Option<Result<(), String>> {
        self.result.lock().unwrap().as_ref().map(|r| r.as_ref().map(|_| ()).map_err(Clone::clone))
    }
    pub(super) fn text(&self) -> Option<String> {
        self.result.lock().unwrap().as_ref().and_then(|r| r.as_ref().ok().cloned().flatten())
    }
    pub(super) fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}
