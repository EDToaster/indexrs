use std::collections::HashMap;

use indexrs_core::error::IndexError;
use indexrs_core::registry;

pub async fn run_web(port: u16) -> Result<(), IndexError> {
    let config = registry::load_config()?;
    let mut repos = HashMap::new();
    for entry in &config.repo {
        let name = entry.effective_name().to_string();
        repos.insert(name, entry.path.clone());
    }
    if repos.is_empty() {
        eprintln!("warning: no repos registered. Use 'indexrs repos add <path>' to add one.");
    }
    let daemon_bin = std::env::current_exe().map_err(IndexError::Io)?;
    indexrs_web::start_server(repos, daemon_bin, port)
        .await
        .map_err(|e| IndexError::Io(std::io::Error::other(e.to_string())))
}
