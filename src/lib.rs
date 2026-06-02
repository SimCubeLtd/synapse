//! SimCube Synapse library crate.
//!
//! The CLI binary (`src/main.rs`) is a thin shell over these modules. Exposing
//! them as a library lets integration tests in `tests/` call the pure logic
//! (config parsing, glob matching, symbol extraction, the budget fitter, table
//! and markdown rendering) directly, in addition to the end-to-end CLI tests.

pub mod cli;
pub mod config;
pub mod errors;
pub mod explore;
pub mod git;
pub mod graph;
pub mod indexer;
pub mod output;
pub mod pack;
pub mod repo;
#[cfg(feature = "share")]
pub mod share;
