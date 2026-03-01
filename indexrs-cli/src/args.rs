use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Local code search index — fast grep, file, and symbol search for your repositories.
#[derive(Debug, Parser)]
#[command(name = "indexrs", version, about = "Local code search index")]
pub struct Cli {
    /// Color output mode
    #[arg(long, value_enum, default_value_t = ColorMode::Auto, global = true)]
    pub color: ColorMode,

    /// Repository root path (default: auto-detect from cwd)
    #[arg(short = 'r', long, value_name = "PATH", global = true)]
    pub repo: Option<PathBuf>,

    /// Increase verbosity (can repeat: -vv for debug)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

/// Color output mode
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorMode {
    /// Automatic: color when stdout is a TTY
    Auto,
    /// Always emit color codes
    Always,
    /// Never emit color codes
    Never,
}

/// Sort order for file listing
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum SortOrder {
    /// Sort by file path (default)
    #[default]
    Path,
    /// Sort by modification time (newest first)
    Modified,
    /// Sort by file size (largest first)
    Size,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Search code in indexed files (vimgrep-compatible output)
    Search {
        /// Search query string
        query: String,

        /// Interpret query as a regex pattern
        #[arg(long)]
        regex: bool,

        /// Force case-sensitive matching
        #[arg(long, conflicts_with_all = ["ignore_case", "smart_case"])]
        case_sensitive: bool,

        /// Force case-insensitive matching
        #[arg(short = 'i', long, conflicts_with_all = ["case_sensitive", "smart_case"])]
        ignore_case: bool,

        /// Smart case: case-sensitive if query has uppercase (default)
        #[arg(short = 'S', long, conflicts_with_all = ["case_sensitive", "ignore_case"])]
        smart_case: bool,

        /// Filter by programming language
        #[arg(short = 'l', long, value_name = "LANG")]
        language: Option<String>,

        /// Filter by path glob pattern
        #[arg(short, long, value_name = "PATTERN")]
        path: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long, default_value_t = 1000)]
        limit: usize,

        /// Lines of context around matches
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Print match statistics to stderr
        #[arg(long)]
        stats: bool,
    },

    /// List indexed files (one path per line, fd-compatible)
    Files {
        /// Optional query to filter file names
        query: Option<String>,

        /// Filter by programming language
        #[arg(short = 'l', long, value_name = "LANG")]
        language: Option<String>,

        /// Filter by path glob pattern
        #[arg(short, long, value_name = "PATTERN")]
        path: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long)]
        limit: Option<usize>,

        /// Sort order
        #[arg(long, value_enum, default_value_t = SortOrder::Path)]
        sort: SortOrder,
    },

    /// Search symbols (functions, types, constants)
    Symbols {
        /// Optional query to filter symbols
        query: Option<String>,

        /// Filter by symbol kind (fn, struct, trait, enum, etc.)
        #[arg(short = 'k', long, value_name = "KIND")]
        kind: Option<String>,

        /// Filter by programming language
        #[arg(short = 'l', long, value_name = "LANG")]
        language: Option<String>,

        /// Maximum number of results
        #[arg(short = 'n', long)]
        limit: Option<usize>,
    },

    /// Preview file contents with syntax highlighting for fzf
    Preview {
        /// File to preview
        file: PathBuf,

        /// Center preview on this line
        #[arg(long)]
        line: Option<usize>,

        /// Lines of context above/below
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Highlight this specific line
        #[arg(long)]
        highlight_line: Option<usize>,
    },

    /// Show index status (file count, last update, etc.)
    Status,

    /// Trigger reindex of the repository
    Reindex {
        /// Perform a full reindex (default: incremental)
        #[arg(long)]
        full: bool,
    },

    /// Internal: run as daemon process (hidden from help)
    #[command(name = "daemon-start", hide = true)]
    DaemonStart,
}
