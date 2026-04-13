//! Event-driven SPICE client.
//!
//! The intent is that a GUI (e.g. `virtmanager-rs`) can embed this crate
//! and treat SPICE as a stream of [`ClientEvent`]s plus a small send
//! surface for input. The crate spawns one tokio task per attached
//! channel and multiplexes everything through a single async mailbox.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use capsaicin_client::{SpiceClient, ClientEvent, InputEvent};
//!
//! let mut client = SpiceClient::connect("127.0.0.1:5900", "").await?;
//! client.send_input(InputEvent::KeyDown(0x1e)).await?; // press 'A'
//! while let Some(evt) = client.next_event().await {
//!     match evt {
//!         ClientEvent::Display(_) => { /* redraw */ }
//!         ClientEvent::Closed(_)  => break,
//!         _ => {}
//!     }
//! }
//! # Ok(()) }
//! ```

mod client;
mod display;
mod error;
mod events;
mod inputs;
mod mjpeg;

pub use capsaicin_proto::display::{Head as MonitorHead, SurfaceCreate};
pub use capsaicin_proto::stream::VideoCodec;
pub use client::{SpiceClient, SpiceClientBuilder};
pub use error::{ClientError, Result};
pub use events::{ClientEvent, DisplayEvent, InputEvent, Rect, RegionPixels, SurfaceFormat};
