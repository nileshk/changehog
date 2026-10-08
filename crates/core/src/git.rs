//! Thin, read-only wrapper around the `git` CLI.
//!
//! Every invocation sets `GIT_OPTIONAL_LOCKS=0`, so commands like `git status`
//! never opportunistically rewrite `.git/index` (and never take `index.lock`).
//! That matters because a coding agent is usually running its own git commands
//! in the same repository while we watch it.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

/// One entry of `git log`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    /// Author time, in seconds since the Unix epoch.
    pub time: i64,
    pub subject: String,
}

/// Narrows `git log`. Matching is case-insensitive and literal (not regex).
/// With both set, a commit must match both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Text in the commit message.
    pub message: Option<String>,
    pub author: Option<AuthorFilter>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthorFilter {
    /// The configured `user.email` (or `user.name` if there's no email).
    Me,
    /// Text in the author's name or email.
    Matching(String),
}

impl LogFilter {
    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.author.is_none()
    }
}

pub struct Git {
    root: PathBuf,
    cat_file: Mutex<Option<CatFile>>,
}

impl Git {
    /// Finds the repository (or worktree) containing `path`.
    pub fn discover(path: &Path) -> Result<Self> {
        let out = command(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run git (is it installed?)")?;
        if !out.status.success() {
            bail!("{} is not inside a git repository", path.display());
        }
        let root = PathBuf::from(String::from_utf8(out.stdout)?.trim_end());
        // Watcher events report canonical paths (e.g. /private/var on macOS).
        let root = root.canonicalize().unwrap_or(root);
        Ok(Self {
            root,
            cat_file: Mutex::new(None),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn cmd(&self) -> Command {
        command(&self.root)
    }

    /// The commit HEAD points at, or `None` for an unborn branch.
    pub fn head_commit(&self) -> Result<Option<String>> {
        self.resolve("HEAD")
    }

    /// Resolves `rev` to a commit hash, `None` if it doesn't exist.
    fn resolve(&self, rev: &str) -> Result<Option<String>> {
        let out = self
            .cmd()
            .args(["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
            .output()?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(out.stdout)?.trim().to_string()))
    }

    /// The first parent of `commit`, or `None` for a root commit.
    pub fn parent(&self, commit: &str) -> Result<Option<String>> {
        self.resolve(&format!("{commit}^"))
    }

    /// `<short hash> <subject>` for `commit`.
    pub fn summary(&self, commit: &str) -> Result<String> {
        let out = self
            .cmd()
            .args(["log", "-1", "--format=%h %s", commit])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The configured identity for "my commits": `user.email`, or
    /// `user.name` if no email is set.
    pub fn user_identity(&self) -> Result<String> {
        for key in ["user.email", "user.name"] {
            let out = self.cmd().args(["config", "--get", key]).output()?;
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !value.is_empty() {
                return Ok(value);
            }
        }
        bail!("no git user.email or user.name is configured")
    }

    /// The newest `limit` commits reachable from HEAD that match `filter`,
    /// newest first. Empty on an unborn branch.
    pub fn log(&self, limit: usize, filter: &LogFilter) -> Result<Vec<Commit>> {
        let mut cmd = self.cmd();
        cmd.args(["log", "-n", &limit.to_string(), "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s"]);
        if !filter.is_empty() {
            cmd.args(["--regexp-ignore-case", "--fixed-strings"]);
        }
        if let Some(message) = &filter.message {
            cmd.arg(format!("--grep={message}"));
        }
        match &filter.author {
            Some(AuthorFilter::Me) => {
                cmd.arg(format!("--author={}", self.user_identity()?));
            }
            Some(AuthorFilter::Matching(author)) => {
                cmd.arg(format!("--author={author}"));
            }
            None => {}
        }
        let out = cmd.output()?;
        if !out.status.success() {
            return Ok(Vec::new()); // no commits yet
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut f = line.splitn(5, '\x1f');
                Some(Commit {
                    hash: f.next()?.to_string(),
                    short: f.next()?.to_string(),
                    author: f.next()?.to_string(),
                    time: f.next()?.parse().ok()?,
                    subject: f.next()?.to_string(),
                })
            })
            .collect())
    }

    /// Paths changed between `parent` and `commit` (every path in `commit`
    /// when there's no parent).
    pub fn changed_paths(&self, parent: Option<&str>, commit: &str) -> Result<Vec<String>> {
        let mut cmd = self.cmd();
        match parent {
            Some(parent) => cmd.args(["diff", "--name-only", "-z", "--no-renames", parent, commit]),
            None => cmd.args(["ls-tree", "-r", "-z", "--name-only", commit]),
        };
        let out = cmd.output()?;
        if !out.status.success() {
            bail!("git failed listing changes in {commit}");
        }
        Ok(out
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    /// Paths (relative to the root) that differ from HEAD: staged, unstaged,
    /// deleted or untracked. Ignored files are excluded.
    pub fn dirty_paths(&self) -> Result<Vec<String>> {
        let out = self
            .cmd()
            .args([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
            ])
            .output()?;
        if !out.status.success() {
            bail!("git status failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(out
            .stdout
            .split(|&b| b == 0)
            .filter(|entry| entry.len() > 3)
            .map(|entry| String::from_utf8_lossy(&entry[3..]).into_owned())
            .collect())
    }

    /// Returns the subset of `paths` that git considers ignored.
    /// Tracked files are never reported as ignored.
    pub fn ignored(&self, paths: &[String]) -> Result<HashSet<String>> {
        if paths.is_empty() {
            return Ok(HashSet::new());
        }
        let mut child = self
            .cmd()
            .args(["check-ignore", "-z", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let input: Vec<u8> = paths.iter().flat_map(|p| p.bytes().chain([0])).collect();
        // Write from a separate thread so a large batch can't deadlock on pipe buffers.
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child.wait_with_output()?;
        let _ = writer.join();
        // Exit code 1 just means "nothing ignored"; anything else is an error.
        match out.status.code() {
            Some(0) | Some(1) => {}
            _ => bail!("git check-ignore failed"),
        }
        Ok(out
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    /// Reads `path` as it exists in `commit`. `None` if it isn't there.
    pub fn blob(&self, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
        let mut guard = self.cat_file.lock().unwrap();
        if guard.is_none() {
            *guard = Some(CatFile::spawn(&self.root)?);
        }
        let result = guard.as_mut().unwrap().read(&format!("{commit}:{path}"));
        if result.is_err() {
            // The batch process is in an unknown state; respawn next time.
            *guard = None;
        }
        result
    }
}

fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    cmd
}

/// A long-lived `git cat-file --batch` process, so reading many blobs doesn't
/// cost a process spawn each.
struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    fn spawn(root: &Path) -> Result<Self> {
        let mut child = command(root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn read(&mut self, spec: &str) -> Result<Option<Vec<u8>>> {
        if spec.contains('\n') {
            return Ok(None);
        }
        writeln!(self.stdin, "{spec}")?;
        self.stdin.flush()?;

        let mut header = String::new();
        if self.stdout.read_line(&mut header)? == 0 {
            bail!("git cat-file exited unexpectedly");
        }
        // "<oid> <type> <size>" or "<spec> missing" / "<spec> ambiguous".
        let mut parts = header.trim_end().rsplitn(3, ' ');
        let (Some(size), Some(kind)) = (parts.next(), parts.next()) else {
            return Ok(None);
        };
        let Ok(size) = size.parse::<usize>() else {
            return Ok(None); // "missing" or "ambiguous"
        };
        let mut buf = vec![0; size + 1]; // content plus trailing newline
        self.stdout.read_exact(&mut buf)?;
        buf.pop();
        Ok((kind == "blob").then_some(buf))
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
