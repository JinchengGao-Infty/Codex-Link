use std::future::Future;
use std::pin::Pin;

use codex_protocol::protocol::EventMsg;

use crate::ExtensionData;

/// Future returned by one turn-event callback.
pub type TurnEventFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Input supplied when the host emits one model/tool/runtime event for a turn.
pub struct TurnEventInput<'a> {
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Event being emitted by the host.
    pub event: &'a EventMsg,
}
