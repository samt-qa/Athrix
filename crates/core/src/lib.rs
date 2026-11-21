//! Shared types, errors and logging for Athrix.

pub mod error;
pub mod logging;

pub use error::{Error, Result};

/// Application name, as shown to users and used for the CLI binary.
pub const APP_NAME: &str = "Athrix";

/// Version of the Athrix workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
