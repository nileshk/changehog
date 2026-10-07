//! A record of everything that changed during the session, kept so frontends
//! can later offer replay and catch-up.

use std::collections::{HashMap, VecDeque};
use std::time::SystemTime;

use crate::baseline::Content;

/// Revisions kept per file; older ones are dropped.
const MAX_REVISIONS_PER_FILE: usize = 64;

#[derive(Clone, Debug)]
pub struct Change {
    pub seq: u64,
    pub at: SystemTime,
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Clone, Debug)]
pub struct Revision {
    pub seq: u64,
    pub content: Content,
}

#[derive(Default)]
pub struct Timeline {
    changes: Vec<Change>,
    revisions: HashMap<String, VecDeque<Revision>>,
}

impl Timeline {
    pub fn record(&mut self, change: Change, content: Content) {
        let revs = self.revisions.entry(change.path.clone()).or_default();
        if revs.len() == MAX_REVISIONS_PER_FILE {
            revs.pop_front();
        }
        revs.push_back(Revision {
            seq: change.seq,
            content,
        });
        self.changes.push(change);
    }

    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    pub fn revisions(&self, path: &str) -> impl Iterator<Item = &Revision> {
        self.revisions.get(path).into_iter().flatten()
    }
}
