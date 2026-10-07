//! Core engine for diff-live: watches a git working tree and produces live
//! diffs. Frontend-agnostic; the TUI (and a future GUI) consume [`Session`]
//! events and drive a [`Director`].

pub mod baseline;
pub mod diff;
pub mod director;
pub mod git;
pub mod session;
pub mod timeline;
pub mod watch;

pub use baseline::{BaseMode, Content};
pub use diff::{Body, DiffLine, FileDiff, FileStatus, LineKind};
pub use director::{Director, DirectorConfig, Flip};
pub use session::{Session, SessionEvent};
