//! Strategies that hang on the core's ports. Nothing here is in the loop body:
//! the loop asks, these answer.

pub mod approval;
pub mod compact;
pub(crate) mod oneshot;
pub mod retry;
pub mod summarize;
