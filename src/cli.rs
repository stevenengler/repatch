use std::path::PathBuf;

use clap::Parser;

const VERSION_STR: &str = concat!("re:patch ", env!("CARGO_PKG_VERSION"));

/// re:patch is a line-oriented find-and-replace tool with a `git add --patch`-like interface.
/// Directories are searched recursively. Hidden files/directories and binary files are ignored, as
/// well as files/directories specified in gitignore rules. Regular expressions with capture groups
/// are supported.
#[derive(Debug, Parser)]
#[command(version, name = "re:patch", max_term_width = 120, help_expected = true)]
#[command(before_help(VERSION_STR))]
pub struct Args {
    /// Regex to search for, optionally with capture groups.
    pub find: String,
    /// Text to replace `<FIND>` with. Capture group indices and names are supported.
    pub replace: String,
    /// Paths (files and/or directories) to search recursively.
    #[clap(required = true)]
    pub paths: Vec<PathBuf>,
    /// Case-insensitive search.
    #[clap(long, short)]
    pub ignore_case: bool,
    /// Ignore filesystem-related errors while searching ("no such file", "permission denied", etc).
    #[clap(long)]
    pub ignore_errors: bool,
    /// Generate diffs with `<N>` lines of context; also accepts "infinite".
    #[clap(long, default_value_t, value_name = "N")]
    pub context: Context,
    /// Show the changes without modifying any files.
    ///
    /// This does not generate valid patch files and is meant only for terminal output. ANSI escape
    /// sequences are replaced in the generated patches.
    #[clap(long, conflicts_with_all(["apply"]))]
    pub show: bool,
    /// Apply and write all changes automatically without any user input or confirmation.
    #[clap(long)]
    pub apply: bool,
}

#[derive(Copy, Clone, Debug)]
pub enum Context {
    Num(u64),
    Infinite,
}

impl std::str::FromStr for Context {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "infinite" => Self::Infinite,
            x => Self::Num(x.parse()?),
        })
    }
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Num(x) => write!(f, "{x}"),
            Self::Infinite => write!(f, "infinite"),
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::Num(5)
    }
}
