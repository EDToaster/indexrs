pub mod client;
pub mod json_protocol;
pub mod types;
pub mod wire;

pub use client::{ensure_daemon, socket_path, try_connect};
pub use json_protocol::{
    FileResponse, HealthResponse, JsonSearchFrame, SearchStats, StatusResponse,
};
pub use types::{DaemonRequest, DaemonResponse};
