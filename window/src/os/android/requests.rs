//! Requests the GUI thread makes of the platform.
//!
//! The GUI thread never calls into Java.  It queues a typed request here
//! and a Kotlin thread blocked in [`PlatformRequests::next`] carries it to
//! the UI thread, so a UI thread that waits for the GUI thread is never
//! waited on in return.
//!
//! A request that expects an answer is owned here from the call that makes
//! it until the answer arrives or the engine ends, never by a GUI-thread
//! task: [`PlatformRequests::close`] fails it whatever that thread got to.

#![forbid(unsafe_code)]

use super::monitor::surface_monitor;
use promise::{Future, Promise};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlatformRequest {
    /// Read the clipboard and answer with `request`.
    ClipboardGet {
        request: u64,
    },
    ClipboardSet {
        text: String,
    },
    /// A logical window appeared, went away, was renamed or was bound.
    WindowsChanged,
    /// The connection phase or its pending prompt changed.
    ConnectionChanged,
}

impl PlatformRequest {
    /// The platform re-reads state when it sees one of these, so two in a
    /// row say nothing more than one.
    fn is_refresh(&self) -> bool {
        matches!(self, Self::WindowsChanged | Self::ConnectionChanged)
    }
}

/// The platform did not answer a clipboard read with text.
#[derive(Debug, thiserror::Error)]
#[error("the Android clipboard is unavailable or holds no text")]
pub struct ClipboardUnavailable;

struct Mailbox {
    queue: VecDeque<PlatformRequest>,
    /// Clipboard reads the platform has not answered, by request id.
    clipboard_reads: BTreeMap<u64, Promise<String>>,
    next_clipboard_read: u64,
}

pub struct PlatformRequests {
    /// `None` once the engine ended.
    mailbox: Mutex<Option<Mailbox>>,
    changed: Condvar,
}

static REQUESTS: PlatformRequests = PlatformRequests {
    mailbox: Mutex::new(Some(Mailbox {
        queue: VecDeque::new(),
        clipboard_reads: BTreeMap::new(),
        next_clipboard_read: 1,
    })),
    changed: Condvar::new(),
};

pub fn platform_requests() -> &'static PlatformRequests {
    &REQUESTS
}

impl PlatformRequests {
    /// Queue `request`; dropped when the engine ended and nobody will read it.
    pub(super) fn push(&self, request: PlatformRequest) {
        let mut mailbox = self.mailbox.lock().unwrap();
        let Some(mailbox) = mailbox.as_mut() else {
            return;
        };
        let repeated = request.is_refresh() && mailbox.queue.back() == Some(&request);
        if !repeated {
            mailbox.queue.push_back(request);
            self.changed.notify_all();
        }
    }

    /// Tell the platform to re-read the connection state.  Callable from
    /// any thread.
    pub fn connection_changed(&self) {
        self.push(PlatformRequest::ConnectionChanged);
    }

    /// Ask the platform for the clipboard's text.  Callable from any thread.
    pub(super) fn read_clipboard(&self) -> Future<String> {
        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();
        {
            let mut mailbox = self.mailbox.lock().unwrap();
            let Some(mailbox) = mailbox.as_mut() else {
                return Future::err(ClipboardUnavailable.into());
            };
            let request = mailbox.next_clipboard_read;
            mailbox.next_clipboard_read += 1;
            mailbox.clipboard_reads.insert(request, promise);
            mailbox
                .queue
                .push_back(PlatformRequest::ClipboardGet { request });
            self.changed.notify_all();
        }
        surface_monitor().update(|s| s.clipboard_requests += 1);
        future
    }

    /// The platform answered clipboard read `request`.  An answer to a
    /// read that is not pending (a duplicate, or the engine ended) is
    /// ignored.
    pub fn complete_clipboard(&self, request: u64, text: Option<String>) {
        let mut mailbox = self.mailbox.lock().unwrap();
        let pending = mailbox
            .as_mut()
            .and_then(|mailbox| mailbox.clipboard_reads.remove(&request));
        drop(mailbox);
        let Some(mut promise) = pending else {
            log::warn!("ignoring clipboard answer {request}: no such pending read");
            return;
        };
        promise.result(text.ok_or_else(|| ClipboardUnavailable.into()));
        surface_monitor().update(|s| s.clipboard_responses += 1);
    }

    /// The engine ended: drop what is queued, fail every unanswered read
    /// and release every reader.  Failing a read only wakes its task; no
    /// GUI-thread task runs here.
    pub fn close(&self) {
        let ended = self.mailbox.lock().unwrap().take();
        self.changed.notify_all();
        for mut promise in ended
            .into_iter()
            .flat_map(|m| m.clipboard_reads.into_values())
        {
            promise.err(ClipboardUnavailable.into());
        }
    }

    /// Block until a request is queued; `None` once the engine ended.
    pub fn next(&self) -> Option<PlatformRequest> {
        let mailbox = self.mailbox.lock().unwrap();
        let mut mailbox = self
            .changed
            .wait_while(mailbox, |mailbox| {
                mailbox.as_ref().is_some_and(|m| m.queue.is_empty())
            })
            .unwrap();
        mailbox.as_mut()?.queue.pop_front()
    }
}
