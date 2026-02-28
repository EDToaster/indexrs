mod args;
mod color;
mod output;

use args::{Cli, Command};
use clap::Parser;

#[tokio::main]
async fn main() {
    output::setup_sigpipe();

    let cli = Cli::parse();

    match cli.command {
        Command::Search {
            query,
            regex: _,
            case_sensitive: _,
            ignore_case: _,
            smart_case: _,
            language: _,
            path: _,
            limit: _,
            context: _,
            stats: _,
        } => {
            println!("TODO: implement search (query: {query:?})");
        }
        Command::Files {
            query,
            language: _,
            path: _,
            limit: _,
            sort: _,
        } => {
            println!("TODO: implement files (query: {query:?})");
        }
        Command::Symbols {
            query,
            kind: _,
            language: _,
            limit: _,
        } => {
            println!("TODO: implement symbols (query: {query:?})");
        }
        Command::Preview {
            file,
            line: _,
            context: _,
            highlight_line: _,
        } => {
            println!("TODO: implement preview (file: {})", file.display());
        }
        Command::Status => {
            println!("TODO: implement status");
        }
        Command::Reindex { full } => {
            println!(
                "TODO: implement reindex (mode: {})",
                if full { "full" } else { "incremental" }
            );
        }
    }
}
