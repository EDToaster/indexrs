pub mod client;
pub mod json_protocol;
pub mod types;
pub mod wire;

pub use client::{JsonResult, ensure_daemon, send_json_request, socket_path, try_connect};
pub use json_protocol::{
    FileResponse, HealthResponse, JsonSearchFrame, SearchStats, SegmentInfo, StatusResponse,
};
pub use types::{DaemonRequest, DaemonResponse};
