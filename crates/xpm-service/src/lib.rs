//! Business logic and orchestration for xPackageManager.

pub mod manager;
pub mod state;

pub use manager::PackageManager;
pub use state::{AppState, ViewState};
