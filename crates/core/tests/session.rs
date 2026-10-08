use std::path::Path;
use std::process::Command;
use std::time::Duration;

use changehog_core::{BaseMode, FileStatus, Session, SessionEvent};

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

/// The next event that isn't a fallback update.
fn next_change(session: &Session) -> SessionEvent {
    loop {
        match next(session) {
            SessionEvent::Fallback { .. } => continue,
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
