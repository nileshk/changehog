use std::path::Path;
use std::process::Command;
use std::time::Duration;

use changehog_core::{
    AuthorFilter, BaseMode, FileStatus, LogFilter, Session, SessionEvent, UNCOMMITTED, WorkingTree,
};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "git {args:?}");
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

fn next(session: &Session) -> SessionEvent {
    session
        .events()
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for a session event")
}

/// The next event that isn't a fallback or log update.
fn next_change(session: &Session) -> SessionEvent {
    loop {
        match next(session) {
            SessionEvent::Fallback { .. } | SessionEvent::Log { .. } | SessionEvent::WorkingTree(_) => {
                continue;
            }
            other => return other,
        }
    }
}

fn fallback(session: &Session) -> (String, Vec<String>) {
    loop {
        if let SessionEvent::Fallback { label, diffs } = next(session) {
            return (label, diffs.iter().map(|d| d.path.clone()).collect());
        }
    }
}

fn settle() {
    // Give FSEvents time to start delivering before we write.
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn reports_edits_against_session_start() {
    let dir = repo();
    // Dirty before the session starts: becomes part of the baseline.
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\n").unwrap();
    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    settle();

    std::fs::create_dir_all(dir.path().join("build")).unwrap();
    std::fs::write(dir.path().join("build/out.o"), "ignored").unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();

    let SessionEvent::Changed(d) = next_change(&session) else { panic!() };
    assert_eq!(d.path, "a.txt");
    assert_eq!((d.added, d.removed), (1, 0), "only the post-start edit");
    assert!(d.lines()[d.focus.unwrap()].fresh);

    // The agent commits mid-session: the diff must not vanish.
    git(dir.path(), &["commit", "-qam", "agent"]);
    std::fs::write(dir.path().join("new.rs"), "fn main() {}\n").unwrap();
    let SessionEvent::Changed(d) = next_change(&session) else { panic!() };
    assert_eq!(d.path, "new.rs");
    assert_eq!(d.status, FileStatus::Added);

    // Restoring the start contents reports a revert.
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\n").unwrap();
    assert!(matches!(next_change(&session), SessionEvent::Reverted(p) if p == "a.txt"));

    // We never touched the index lock along the way.
    assert!(!dir.path().join(".git/index.lock").exists());
}

#[test]
fn head_mode_reports_existing_changes() {
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "one\nthree\n").unwrap();
    let session = Session::start(dir.path(), BaseMode::Head).unwrap();
    let SessionEvent::Changed(d) = next(&session) else { panic!() };
    assert_eq!((d.path.as_str(), d.added, d.removed), ("a.txt", 0, 1));
    assert!(d.lines().iter().all(|l| !l.fresh), "pre-existing changes aren't fresh");
}

#[test]
fn clean_tree_falls_back_to_last_commit() {
    let dir = repo();
    std::fs::write(dir.path().join("b.txt"), "bee\n").unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "second commit"]);

    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    let (label, paths) = fallback(&session);
    assert!(label.starts_with("last commit") && label.ends_with("second commit"), "{label}");
    assert_eq!(paths, ["a.txt", "b.txt"]);
    settle();

    // Work committed during the session is a session change, not fallback.
    std::fs::write(dir.path().join("c.txt"), "sea\n").unwrap();
    git(dir.path(), &["add", "c.txt"]);
    git(dir.path(), &["commit", "-qm", "third"]);
    assert!(matches!(next_change(&session), SessionEvent::Changed(d) if d.path == "c.txt"));
}

#[test]
fn dirty_tree_falls_back_to_uncommitted_changes() {
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    let (label, paths) = fallback(&session);
    assert_eq!(label, "uncommitted changes vs HEAD");
    assert_eq!(paths, ["a.txt"]);
}

#[test]
fn fallback_returns_after_last_change_is_reverted() {
    let dir = repo();
    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    let (label, _) = fallback(&session);
    assert!(label.ends_with("init"), "{label}");
    settle();

    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    assert!(matches!(next_change(&session), SessionEvent::Changed(_)));
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    assert!(matches!(next_change(&session), SessionEvent::Reverted(_)));
    // Same fallback as before, so it isn't resent; nothing else arrives.
    assert!(session.events().recv_timeout(Duration::from_millis(500)).is_err());
}

fn log(session: &Session) -> Vec<String> {
    loop {
        if let SessionEvent::Log { commits, .. } = next(session) {
            return commits.iter().map(|c| c.subject.clone()).collect();
        }
    }
}

#[test]
fn log_lists_commits_and_updates_on_commit() {
    let dir = repo();
    std::fs::write(dir.path().join("b.txt"), "bee\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "second"]);

    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    assert_eq!(log(&session), ["second", "init"], "newest first");
    settle();

    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "third"]);
    assert_eq!(log(&session), ["third", "second", "init"]);
}

#[test]
fn load_commit_diffs_against_its_parent() {
    let dir = repo();
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "bee\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "second"]);

    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    let hashes: Vec<String> = loop {
        if let SessionEvent::Log { commits, .. } = next(&session) {
            break commits.iter().map(|c| c.hash.clone()).collect();
        }
    };
    // The root commit: every file is added.
    session.load_commit(&hashes[1]);
    let (label, diffs) = loop {
        if let SessionEvent::Commit { hash, label, diffs } = next(&session) {
            assert_eq!(hash, hashes[1]);
            break (label, diffs);
        }
    };
    assert!(label.starts_with("commit ") && label.ends_with("init"), "{label}");
    let summary: Vec<_> = diffs.iter().map(|d| (d.path.as_str(), d.status, d.added, d.removed)).collect();
    assert_eq!(
        summary,
        [(".gitignore", FileStatus::Added, 1, 0), ("a.txt", FileStatus::Added, 3, 0)]
    );

    session.load_commit(&hashes[0]);
    let diffs = loop {
        if let SessionEvent::Commit { diffs, .. } = next(&session) {
            break diffs;
        }
    };
    let summary: Vec<_> = diffs.iter().map(|d| (d.path.as_str(), d.added, d.removed)).collect();
    assert_eq!(summary, [("a.txt", 1, 1), ("b.txt", 1, 0)]);
    assert!(diffs.iter().all(|d| d.lines().iter().all(|l| !l.fresh)));
}

fn commit_as(dir: &Path, name: &str, message: &str) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={name}@example.com")])
        .args(["-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", message])
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn log_filters_by_message_author_and_me() {
    let dir = repo();
    commit_as(dir.path(), "alice", "Fix the parser");
    commit_as(dir.path(), "bob", "fix typo in README");
    commit_as(dir.path(), "alice", "Add feature");
    git(dir.path(), &["config", "user.email", "bob@example.com"]);

    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    assert_eq!(log(&session).len(), 4);

    let filtered = |filter: LogFilter| {
        session.set_log_filter(filter.clone());
        loop {
            if let SessionEvent::Log { filter: f, commits } = next(&session)
                && f == filter
            {
                return commits.iter().map(|c| c.subject.clone()).collect::<Vec<_>>();
            }
        }
    };
    let message = |m: &str| Some(m.to_string());
    let author = |a: &str| Some(AuthorFilter::Matching(a.to_string()));

    // Case-insensitive, and literal: "." isn't a wildcard.
    assert_eq!(filtered(LogFilter { message: message("FIX"), author: None }), ["fix typo in README", "Fix the parser"]);
    assert!(filtered(LogFilter { message: message("f.x"), author: None }).is_empty());
    assert_eq!(filtered(LogFilter { message: None, author: author("ALICE") }), ["Add feature", "Fix the parser"]);
    assert_eq!(filtered(LogFilter { message: None, author: Some(AuthorFilter::Me) }), ["fix typo in README"]);
    // Both must match.
    assert_eq!(filtered(LogFilter { message: message("fix"), author: author("alice") }), ["Fix the parser"]);
    assert_eq!(filtered(LogFilter::default()).len(), 4);
}

fn working_tree(session: &Session) -> Option<WorkingTree> {
    loop {
        if let SessionEvent::WorkingTree(tree) = next(session) {
            return tree;
        }
    }
}

#[test]
fn reports_uncommitted_changes() {
    let dir = repo();
    let session = Session::start(dir.path(), BaseMode::SessionStart).unwrap();
    assert_eq!(working_tree(&session), None, "clean at start");
    settle();

    // One staged edit (+1 -1), one unstaged on top of it (+1), one new file (+2).
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    std::fs::write(dir.path().join("new.txt"), "x\ny\n").unwrap();
    let tree = loop {
        // Intermediate summaries may arrive as the edits land.
        if let Some(tree) = working_tree(&session)
            && tree.files == 2
            && tree.added == 4
        {
            break tree;
        }
    };
    assert_eq!(
        tree,
        WorkingTree { files: 2, staged: 1, unstaged: 1, untracked: 1, added: 4, removed: 1 }
    );

    session.load_commit(UNCOMMITTED);
    let (label, diffs) = loop {
        if let SessionEvent::Commit { hash, label, diffs } = next(&session) {
            assert_eq!(hash, UNCOMMITTED);
            break (label, diffs);
        }
    };
    assert_eq!(label, "uncommitted changes vs HEAD");
    let summary: Vec<_> = diffs.iter().map(|d| (d.path.as_str(), d.added, d.removed)).collect();
    assert_eq!(summary, [("a.txt", 2, 1), ("new.txt", 2, 0)]);

    // Committing everything makes it clean again.
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "all"]);
    while working_tree(&session).is_some() {}
}
