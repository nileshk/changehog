//! What each file is compared against.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::git::Git;

/// Files larger than this are reported as changed but not diffed.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// The contents of one file at some moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Absent,
    Bytes(Arc<[u8]>),
    TooLarge(u64),
}

impl Content {
    /// Reads a file from disk, treating any read failure as absent (the file
    /// may have been deleted or renamed between the event and the read).
    pub fn read(path: &Path) -> Content {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                if meta.len() > MAX_FILE_BYTES {
                    return Content::TooLarge(meta.len());
                }
                match std::fs::read(path) {
                    Ok(bytes) => Content::Bytes(bytes.into()),
                    Err(_) => Content::Absent,
                }
            }
            _ => Content::Absent,
        }
    }

    pub(crate) fn from_blob(blob: Option<Vec<u8>>) -> Content {
        match blob {
            None => Content::Absent,
            Some(b) if b.len() as u64 > MAX_FILE_BYTES => Content::TooLarge(b.len() as u64),
            Some(b) => Content::Bytes(b.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BaseMode {
    /// The working tree as it was when the session started. Survives the
    /// agent committing, stashing or resetting mid-session.
    #[default]
    SessionStart,
    /// The HEAD commit as of session start (pinned, so later commits don't
    /// make changes vanish).
    Head,
}

pub struct Baseline {
    mode: BaseMode,
    head: Option<String>,
    /// Session-start contents of files that were already dirty at startup.
    snapshot: HashMap<String, Content>,
    /// Lazily loaded HEAD blobs.
    cache: HashMap<String, Content>,
}

impl Baseline {
    /// Captures the baseline. Returns it along with the paths that were dirty
    /// relative to HEAD at startup.
    pub fn capture(git: &Git, mode: BaseMode) -> Result<(Self, Vec<String>)> {
        let head = git.head_commit()?;
        let dirty = git.dirty_paths()?;
        let snapshot = match mode {
            BaseMode::SessionStart => dirty
                .iter()
                .map(|p| (p.clone(), Content::read(&git.root().join(p))))
                .collect(),
            BaseMode::Head => HashMap::new(),
        };
        let baseline = Self {
            mode,
            head,
            snapshot,
            cache: HashMap::new(),
        };
        Ok((baseline, dirty))
    }

    pub fn mode(&self) -> BaseMode {
        self.mode
    }

    pub fn get(&mut self, git: &Git, path: &str) -> Result<Content> {
        if let Some(c) = self.snapshot.get(path).or_else(|| self.cache.get(path)) {
            return Ok(c.clone());
        }
        let content = match &self.head {
            Some(commit) => Content::from_blob(git.blob(commit, path)?),
            None => Content::Absent,
        };
        self.cache.insert(path.to_string(), content.clone());
        Ok(content)
    }
}
