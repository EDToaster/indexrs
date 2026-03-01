mod args;
mod color;
mod daemon;
mod files;
mod output;
mod preview;
mod repo;
mod search_cmd;

use std::io::IsTerminal;

use args::{Cli, ColorMode, Command};
use clap::Parser;
use color::ColorConfig;
use output::{ExitCode, StreamingWriter};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    output::setup_sigpipe();

    let cli = Cli::parse();

    let color_enabled = match cli.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => std::io::stdout().is_terminal(),
    };
    let color = ColorConfig::new(color_enabled);

    let exit_code = match run(cli, &color).await {
        Ok(code) => code as i32,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::Error as i32
        }
    };

    std::process::exit(exit_code);
}

async fn run(cli: Cli, color: &ColorConfig) -> Result<ExitCode, indexrs_core::IndexError> {
    match cli.command {
        Command::Search {
            query,
            regex,
            case_sensitive,
            ignore_case,
            smart_case,
            language,
            path,
            limit,
            context,
            stats: _,
        } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;

            // Resolve smart case: daemon uses explicit case flags, not smart_case.
            let (eff_case_sensitive, eff_ignore_case) = if case_sensitive {
                (true, false)
            } else if ignore_case {
                (false, true)
            } else if smart_case || (!case_sensitive && !ignore_case) {
                // Smart case default: case-sensitive if query has uppercase
                if query.chars().any(|c| c.is_uppercase()) {
                    (true, false)
                } else {
                    (false, true)
                }
            } else {
                (false, true)
            };

            // Trigram search requires at least 3 characters for non-regex queries.
            if !regex && query.len() < 3 {
                eprintln!(
                    "warning: search query must be at least 3 characters (got {})",
                    query.len()
                );
                return Ok(ExitCode::NoResults);
            }

            let request = daemon::DaemonRequest::Search {
                query,
                regex,
                case_sensitive: eff_case_sensitive,
                ignore_case: eff_ignore_case,
                limit,
                context_lines: context.unwrap_or(0),
                language,
                path_glob: path,
                color: color.enabled,
            };

            let stdout = std::io::stdout();
            let mut writer = StreamingWriter::new(stdout.lock());
            daemon::run_via_daemon(&repo_root, request, &mut writer).await
        }
        Command::Files {
            query: _,
            language,
            path,
            limit,
            sort,
        } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;

            let sort_str = match sort {
                args::SortOrder::Path => "path",
                args::SortOrder::Modified => "modified",
                args::SortOrder::Size => "size",
            };

            let request = daemon::DaemonRequest::Files {
                language,
                path_glob: path,
                sort: sort_str.to_string(),
                limit,
                color: color.enabled,
            };

            let stdout = std::io::stdout();
            let mut writer = StreamingWriter::new(stdout.lock());
            daemon::run_via_daemon(&repo_root, request, &mut writer).await
        }
        Command::Preview {
            file,
            line,
            context,
            highlight_line,
        } => {
            let opts = preview::PreviewOptions {
                file,
                line,
                context,
                highlight_line,
                color_enabled: color.enabled,
            };
            preview::run_preview(&opts)?;
            Ok(ExitCode::Success)
        }
        Command::Symbols { .. } => {
            eprintln!("symbols: not yet implemented (post-v0.2)");
            Ok(ExitCode::Error)
        }
        Command::Status => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            let manager = repo::load_index(&repo_root)?;
            let snapshot = manager.snapshot();
            let file_count: usize = snapshot.iter().map(|s| s.entry_count() as usize).sum();
            println!("Segments: {}", snapshot.len());
            println!("Files: {file_count}");
            Ok(ExitCode::Success)
        }
        Command::Reindex { full: _ } => {
            eprintln!("reindex: not yet implemented");
            Ok(ExitCode::Error)
        }
        Command::DaemonStart => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            daemon::start_daemon(&repo_root).await?;
            Ok(ExitCode::Success)
        }
    }
}
