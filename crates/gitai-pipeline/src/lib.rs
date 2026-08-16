//! The orchestration layer: five roles, two nested loops, one budget.
//!
//! Read [`engine`] first. Everything else here exists to keep that module
//! about the loop rather than about JSON parsing, prompt rendering or context
//! trimming.

pub mod context;
pub mod edits;
pub mod engine;
pub mod prompts;
pub mod roles;
pub mod testing;
pub mod web_search;

pub use edits::{Edit, EditOutcome};
pub use engine::Engine;
pub use prompts::Prompts;
pub use roles::Roles;
pub use web_search::{SearchResult, WebSearchEngine};
