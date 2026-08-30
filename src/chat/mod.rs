//! What a call is made of.
//!
//! A request, a reply, and the turns in between. These three files are read together and
//! change together, which is why they sit together.

pub mod message;
pub mod request;
pub mod response;
pub mod stream;

pub use message::{ContentBlock, ImageSource, Message, Role, StopReason};
pub use request::{ChatRequest, Effort, Generation, Needs, Thinking, ToolSchema};
pub use response::ChatResponse;
pub use stream::{Event, EventStream, Transcript};
