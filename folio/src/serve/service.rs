//! What a service of folio serve's is: given a connection its machine routed to the service's
//! socket, as the bottle it's from, it answers it, and says how that went. A service knows nothing
//! of the others, nor of how connections reach it (mod.rs's): one socket, one service.

use std::os::unix::net::UnixStream;
use std::sync::Arc;

use super::Bottle;
use super::proxy::Header;

/// The bottles folio serve answers, as a service may ask after them.
pub trait Bound {
    /// The bottle bound as `name`, if it's bound.
    fn bound(&self, name: &str) -> Option<Arc<Bottle>>;
    /// Every bottle bound, by name.
    fn all(&self) -> Vec<Arc<Bottle>>;
}

/// Who a connection is from (the bottle its machine was bound as), what it was for (its PROXY
/// header: where it went, and the name it asked for), and the other bottles.
pub struct Caller<'a> {
    pub bottle: &'a Arc<Bottle>,
    pub header: &'a Header,
    pub bound: &'a dyn Bound,
}

/// How a connection went, for folio serve's log: what it reached, how it ended, a failure's status,
/// the bytes each way, and whether a credential was put in it.
#[derive(Debug, Default)]
pub struct Flow {
    pub to: String,
    pub outcome: String,
    pub status: Option<String>,
    pub up: u64,
    pub down: u64,
    pub injected: bool,
}

impl Flow {
    pub fn new(to: &str, outcome: impl Into<String>) -> Flow {
        Flow {
            to: to.into(),
            outcome: outcome.into(),
            ..Flow::default()
        }
    }
}

/// A service: what it does with a connection routed to it.
pub trait Service: Send + Sync {
    fn serve(&self, caller: &Caller, stream: UnixStream) -> Flow;
}
