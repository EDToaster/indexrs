pub mod client;
pub mod json_protocol;
pub mod types;
pub mod wire;

pub use client::{JsonResult, ensure_daemon, send_json_request, socket_path, try_connect};
pub use json_protocol::{
    FileResponse, HealthResponse, JsonSearchFrame, JsonSymbolsFrame, SearchStats, SegmentInfo,
    StatusResponse, SymbolMatchResponse, SymbolsStats,
};
pub use types::{CaseMode, DaemonRequest, DaemonResponse};
