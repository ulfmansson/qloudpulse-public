//! A small AWS client: builds each protocol's request from the call plan, signs it with SigV4, sends it, retries
//! throttling, and pages. It reads no answer into types: answers are recorded raw (see [`crate::record`]).

pub mod caller;
pub mod credentials;
pub mod protocol;
pub mod sign;
pub mod transport;

pub use caller::Caller;
pub use transport::{HttpRequest, HttpResponse, Transport, UreqTransport};
