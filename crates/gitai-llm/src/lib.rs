//! Model providers and the router in front of them.
//!
//! Three adapters cover the field: an OpenAI chat-completions client (which is
//! also how every local runtime is reached), an Anthropic client, and a mock
//! that keeps the test suite offline.

pub mod anthropic;
pub mod json;
pub mod mock;
pub mod openai;
pub mod registry;
pub mod retry;
pub mod tokens;

pub use json::{extract_json, parse_json};
pub use registry::{Call, Completion, ModelRegistry};
