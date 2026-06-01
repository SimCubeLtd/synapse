//! Graph model, the [`store::GraphStore`] trait, and its backends.

pub mod memory_store;
pub mod model;
pub mod store;

#[cfg(feature = "ladybug")]
pub mod ladybug_store;

pub use store::GraphStore;
