//! Command definitions, typed dispatch, execution, and diagnostic tracing.

pub mod actions;
pub mod commands;
pub mod dispatch;
pub mod telemetry;

mod start;
pub use start::start;
