use std::path::Path;

use indexrs_core::error::IndexError;
use indexrs_core::registry::{add_repo, config_file_path, load_config, remove_repo, save_config};

/// List all registered repositories.
pub fn run_list() -> Result<(), IndexError> {
    let config = load_config()?;

    if config.repo.is_empty() {
        eprintln!("No repositories registered.");
        eprintln!("Use 'indexrs repos add <path>' to register one.");
        return Ok(());
    }

    for entry in &config.repo {
        let name = entry.effective_name();
        let path = entry.path.display();
        let status = if entry.path.join(".indexrs").join("segments").exists() {
            "indexed"
        } else {
            "not indexed"
        };
        println!("{name}\t{path}\t({status})");
    }

    Ok(())
}

/// Register a new repository.
pub fn run_add(path: &Path, name: Option<&str>) -> Result<(), IndexError> {
    // Canonicalize the path so the registry always stores absolute paths.
    let canonical = path.canonicalize().map_err(|e| {
        IndexError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot resolve path '{}': {e}", path.display()),
        ))
    })?;

    // Validate that .indexrs/ exists (i.e. the repo has been initialized).
    if !canonical.join(".indexrs").exists() {
        return Err(IndexError::Config(format!(
            "no .indexrs directory found at '{}'. Run 'indexrs init' first.",
            canonical.display()
        )));
    }

    let mut config = load_config()?;

    let name_override = name.map(|s| s.to_string());
    if !add_repo(&mut config, canonical.clone(), name_override) {
        // Determine whether it was a path duplicate or name collision.
        if config.find_by_path(&canonical).is_some() {
            return Err(IndexError::Config(format!(
                "repository at '{}' is already registered",
                canonical.display()
            )));
        }
        let effective = name.unwrap_or_else(|| {
            canonical
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
        });
        return Err(IndexError::Config(format!(
            "a repository named '{effective}' already exists. Use --name to pick a different name."
        )));
    }

    save_config(&config)?;

    let effective_name = config.repo.last().unwrap().effective_name();
    eprintln!(
        "Registered repo \"{effective_name}\" ({}) in {}",
        canonical.display(),
        config_file_path().display()
    );

    Ok(())
}

/// Unregister a repository by name.
pub fn run_remove(name: &str) -> Result<(), IndexError> {
    let mut config = load_config()?;

    if !remove_repo(&mut config, name) {
        return Err(IndexError::Config(format!(
            "no repository named '{name}' found in registry"
        )));
    }

    save_config(&config)?;
    eprintln!("Unregistered repo \"{name}\".");

    Ok(())
}
