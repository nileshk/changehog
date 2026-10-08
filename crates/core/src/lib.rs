//! Core engine for changehog: watches a git working tree and produces live
//! diffs. Frontend-agnostic; the TUI (and a future GUI) consume [`Session`]
//! events and drive a [`Director`].

pub mod baseline;
pub mod config;
pub mod diff;
pub mod director;
pub mod git;
pub mod session;
pub mod timeline;
pub mod watch;

pub use baseline::{BaseMode, Content};
pub use config::{Config, SidebarMode};
pub use diff::{Body, DiffLine, FileDiff, FileStatus, LineKind};
pub use director::{CYCLE_STEPS, Director, DirectorConfig, Flip, Measure, Transition};
pub use session::{Session, SessionEvent};
