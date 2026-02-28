mod args;
mod color;
#[allow(unused)]
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
    output::setup_sigpipe();

    let cli = Cli::parse();

    let color_enabled = match cli.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => std::io::stdout().is_terminal(),
    };
    let color = ColorConfig::new(color_enabled);

    let exit_code = match run(cli, &color) {
        Ok(code) => code as i32,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::Error as i32
        }
    };

    std::process::exit(exit_code);
}

fn run(cli: Cli, color: &ColorConfig) -> Result<ExitCode, indexrs_core::IndexError> {
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
            stats,
        } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            let manager = repo::load_index(&repo_root)?;
            let snapshot = manager.snapshot();

            let pattern = search_cmd::resolve_match_pattern(
                &query,
                regex,
                case_sensitive,
                ignore_case,
                smart_case,
            );
            let opts = search_cmd::SearchCmdOptions {
                pattern,
                context_lines: context.unwrap_or(0),
                limit,
                language,
                path_glob: path,
                stats,
            };

            let stdout = std::io::stdout();
            let mut writer = StreamingWriter::new(stdout.lock());
            search_cmd::run_search(&snapshot, &opts, color, &mut writer)
        }
        Command::Files {
            query: _,
            language,
            path,
            limit,
            sort,
        } => {
            let repo_root = repo::find_repo_root(cli.repo.as_deref())?;
            let manager = repo::load_index(&repo_root)?;
            let snapshot = manager.snapshot();

            let filter = files::FilesFilter {
                language,
                path_glob: path,
                sort,
                limit,
            };

            let stdout = std::io::stdout();
            let mut writer = StreamingWriter::new(stdout.lock());
            files::run_files(&snapshot, &filter, color, &mut writer)
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
    }
}
