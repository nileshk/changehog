mod app;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use changehog_core::config::{self, CYCLE_RANGE};
use changehog_core::{BaseMode, Config, Session};
use clap::{Parser, ValueEnum};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

/// Live, auto-following diff viewer for watching coding agents work.
///
/// Settings are read from ~/.config/changehog/config.toml (or
/// $XDG_CONFIG_HOME/changehog/config.toml) if it exists; flags override it.
#[derive(Parser)]
#[command(name = "changehog", version)]
struct Args {
    /// Repository (or any path inside it) to watch.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// What to diff against [default: session].
    #[arg(long, value_enum)]
    base: Option<Base>,

    /// Seconds to show each file when cycling (adjust live with +/-)
    /// [default: 4].
    #[arg(long, value_name = "SECONDS", value_parser = parse_cycle)]
    cycle: Option<f32>,

    /// Wrap long lines (toggle live with w).
    #[arg(long, overrides_with = "no_wrap")]
    wrap: bool,

    /// Cut long lines off at the edge instead of wrapping.
    #[arg(long, overrides_with = "wrap")]
    no_wrap: bool,

    /// Read settings from this file instead of the default location.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print a commented config file with every setting at its default,
    /// then exit.
    #[arg(long)]
    print_config: bool,
}

fn parse_cycle(s: &str) -> Result<f32, String> {
    let secs: f32 = s.parse().map_err(|_| format!("`{s}` isn't a number"))?;
    if CYCLE_RANGE.contains(&secs) {
        Ok(secs)
    } else {
        Err(format!(
            "must be between {} and {} seconds",
            CYCLE_RANGE.start(),
            CYCLE_RANGE.end()
        ))
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Base {
    /// The working tree when changehog started (survives mid-session commits).
    Session,
    /// The HEAD commit when changehog started, including pre-existing changes.
    Head,
}

/// Layers command-line flags over the config file's settings.
fn apply_args(mut config: Config, args: &Args) -> Config {
    if let Some(base) = args.base {
        config.base = match base {
            Base::Session => BaseMode::SessionStart,
            Base::Head => BaseMode::Head,
        };
    }
    if let Some(cycle) = args.cycle {
        config.cycle_seconds = cycle;
    }
    if args.wrap {
        config.wrap = true;
    }
    if args.no_wrap {
        config.wrap = false;
    }
    config
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.print_config {
        print!("{}", config::TEMPLATE);
        return Ok(());
    }
    let config = apply_args(Config::load(args.config.as_deref())?, &args);
    let session = Session::start(&args.path, config.base)?;

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    // ratatui's panic hook restores the terminal; release the mouse too.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));

    let result = app::App::new(session, &config).run(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(file: &str, flags: &[&str]) -> Config {
        let args = Args::parse_from(["changehog"].iter().chain(flags));
        apply_args(Config::parse(file).unwrap(), &args)
    }

    #[test]
    fn flags_override_the_file_which_overrides_defaults() {
        let c = merged("", &[]);
        assert_eq!(c, Config::default());

        let file = "cycle_seconds = 8\nwrap = true\nbase = \"head\"";
        let c = merged(file, &[]);
        assert_eq!((c.cycle_seconds, c.wrap, c.base), (8.0, true, BaseMode::Head));

        let c = merged(file, &["--cycle", "2", "--no-wrap", "--base", "session"]);
        assert_eq!((c.cycle_seconds, c.wrap, c.base), (2.0, false, BaseMode::SessionStart));
    }

    #[test]
    fn last_wrap_flag_wins() {
        assert!(merged("", &["--no-wrap", "--wrap"]).wrap);
        assert!(!merged("", &["--wrap", "--no-wrap"]).wrap);
    }
}
