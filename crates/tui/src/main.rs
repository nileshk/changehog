mod app;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use diff_live_core::{BaseMode, Session};

/// Live, auto-following diff viewer for watching coding agents work.
#[derive(Parser)]
#[command(name = "diff-live", version)]
struct Args {
    /// Repository (or any path inside it) to watch.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// What to diff against.
    #[arg(long, value_enum, default_value_t = Base::Session)]
    base: Base,
}

#[derive(Clone, Copy, ValueEnum)]
enum Base {
    /// The working tree when diff-live started (survives mid-session commits).
    Session,
    /// The HEAD commit when diff-live started, including pre-existing changes.
    Head,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mode = match args.base {
        Base::Session => BaseMode::SessionStart,
        Base::Head => BaseMode::Head,
    };
    let session = Session::start(&args.path, mode)?;

    let mut terminal = ratatui::init();
    let result = app::App::new(session).run(&mut terminal);
    ratatui::restore();
    result
}
