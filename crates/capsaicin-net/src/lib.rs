//! SPICE link handshake and message framing over tokio streams.

pub mod auth;
pub mod channel;
pub mod client;
pub mod error;
pub mod link;
pub mod server;
pub mod tls;

pub use channel::{Channel, Message};
pub use client::{MainConnection, connect_sub_channel};
pub use error::{NetError, Result};
pub use link::{LinkOptions, link_client};
pub use server::{AcceptedLink, ServerLinkOptions, link_server};
pub use tls::{SpiceStream, TlsConfig, connect_tls, parse_fingerprint};
