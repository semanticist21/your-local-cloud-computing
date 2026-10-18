pub mod client;
pub mod hub;
pub mod model;
pub mod platform;
pub mod policy;
pub mod project;
pub mod service;
pub mod snapshot;
pub mod worker;

/// `CARGO_PKG_VERSION` for release builds, plus `+g<commit>` for any other build (see build.rs).
pub const VERSION: &str = env!("YLCC_VERSION");
