//! Test homes: a fresh temp folder used as `HOME`, filled from a fixture case, with stub
//! binaries first on `PATH` and a fixed clock. Used by the acceptance tests and AC3.

use ai_dreamcatcher::model::Time;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

pub const NOW: &str = "2026-11-14T12:00:00Z";
pub const SCOPE: &str = "-fixture-work-app";

pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn now() -> Time {
    Time::parse(NOW).unwrap()
}

pub struct Home {
    pub dir: PathBuf,
    /// Put first on `PATH`: links to the stubs a test asked for.
    pub bin: PathBuf,
    pub env: Vec<(String, String)>,
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.dir.parent().unwrap());
    }
}

impl Home {
    /// A home from `tests/fixtures/<case>`, or an empty one. `@HOME@` in file contents becomes
    /// the home path, a `.dc-git-init` file becomes a `git init`, and every file time is set
    /// to the clock minus 2 hours.
    pub fn new(case: Option<&str>) -> Home {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "dreamcatcher-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("home");
        let bin = root.join("bin");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&bin).unwrap();
        // Canonical, so paths match what git and the tool report.
        let dir = fs::canonicalize(&dir).unwrap();
        let bin = fs::canonicalize(&bin).unwrap();
        if let Some(case) = case {
            copy(&repo().join("tests/fixtures").join(case), &dir, &dir);
        }
        let home = Home {
            dir,
            bin,
            env: Vec::new(),
        };
        home.age_files(2 * 3_600);
        home
    }

    /// Links a stub from `tests/stubs` into `bin`.
    pub fn stub(&self, name: &str) {
        let _ =
            std::os::unix::fs::symlink(repo().join("tests/stubs").join(name), self.bin.join(name));
    }

    /// Writes an executable script into `bin`.
    pub fn script(&self, name: &str, text: &str) {
        let path = self.bin.join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.into(), value.into()));
    }

    /// Sets every file's time to the clock minus `secs`.
    pub fn age_files(&self, secs: i64) {
        let t = UNIX_EPOCH + Duration::from_secs((now().0 - secs) as u64);
        for f in walk(&self.dir) {
            let _ = fs::File::options()
                .write(true)
                .open(&f)
                .and_then(|h| h.set_modified(t));
        }
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dreamcatcher"));
        let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.bin.display());
        cmd.args(args)
            .env_clear()
            .env("HOME", &self.dir)
            .env("PATH", path)
            .env("DREAMCATCHER_NOW", NOW)
            .env("DREAMCATCHER_RETRY_DELAY", "0")
            // The python stubs would otherwise cache bytecode under the test HOME.
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("TMPDIR", std::env::temp_dir());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs and asserts the exit code; returns stdout.
    pub fn ok(&self, args: &[&str], code: i32) -> String {
        let out = self.run(args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(
            out.status.code(),
            Some(code),
            "dreamcatcher {args:?}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    pub fn state(&self) -> PathBuf {
        self.dir.join(".local/share/ai-dreamcatcher")
    }

    pub fn store(&self, scope: &str) -> PathBuf {
        self.dir.join(".claude/projects").join(scope).join("memory")
    }

    pub fn ledger(&self) -> Vec<Value> {
        fs::read_to_string(self.state().join("ledger.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Events of kind `ev` in `run`.
    pub fn events(&self, ev: &str, run: &str) -> Vec<Value> {
        self.ledger()
            .into_iter()
            .filter(|e| e["ev"] == ev && e["run"] == run)
            .collect()
    }

    /// Every file under every memory folder, concatenated.
    pub fn store_text(&self) -> String {
        let projects = self.dir.join(".claude/projects");
        walk(&projects)
            .into_iter()
            .filter(|p| p.components().any(|c| c.as_os_str() == "memory"))
            .map(|p| String::from_utf8_lossy(&fs::read(p).unwrap()).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn state_text(&self) -> String {
        walk(&self.state())
            .into_iter()
            .map(|p| String::from_utf8_lossy(&fs::read(p).unwrap()).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A snapshot of every file under the home: path and bytes.
    pub fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut v: Vec<(PathBuf, Vec<u8>)> = walk(&self.dir)
            .into_iter()
            .map(|p| {
                (
                    p.strip_prefix(&self.dir).unwrap().to_owned(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    }

    /// `init --source files` with the stub `claude` and `pmset`.
    pub fn init_files(&self) {
        self.ok(&["init", "--source", "files"], 0);
    }

    /// Checks one AC3 case's `expected.json` against this home after `run`.
    pub fn check(&self, expected: &Value, run: &str) -> Vec<String> {
        let mut fails = Vec::new();
        let attempts = self.events("attempt", run);
        let mut selected: Vec<&Value> = attempts.iter().map(|e| &e["session"]).collect();
        selected.dedup();
        if selected.len() as u64 != expected["selected"].as_u64().unwrap() {
            fails.push(format!(
                "selected {} != {}",
                selected.len(),
                expected["selected"]
            ));
        }
        let done: Vec<Value> = self
            .events("done", run)
            .into_iter()
            .map(|e| e["scope"].clone())
            .collect();
        let applied: Vec<Value> = self
            .events("applied", run)
            .into_iter()
            .filter(|e| done.contains(&e["scope"]))
            .collect();
        for (op, range) in expected["ops"].as_object().unwrap() {
            let n = applied.iter().filter(|e| e["op"] == op.as_str()).count() as u64;
            let (lo, hi) = (range[0].as_u64().unwrap(), range[1].as_u64().unwrap());
            if n < lo || n > hi {
                fails.push(format!("ops.{op} {n} not in [{lo}, {hi}]"));
            }
        }
        let store = self.store_text();
        let state = self.state_text();
        for t in expected["contains"].as_array().unwrap() {
            if !store.contains(t.as_str().unwrap()) {
                fails.push(format!("store lacks {t}"));
            }
        }
        for t in expected["lacks"].as_array().unwrap() {
            if store.contains(t.as_str().unwrap()) {
                fails.push(format!("store contains {t}"));
            }
        }
        for t in expected["state_lacks"].as_array().unwrap() {
            if state.contains(t.as_str().unwrap()) {
                fails.push(format!("state contains {t}"));
            }
        }
        for (t, n) in expected["count"].as_object().unwrap() {
            let got = store.matches(t.as_str()).count() as u64;
            if got != n.as_u64().unwrap() {
                fails.push(format!("count of {t} {got} != {n}"));
            }
        }
        fails
    }
}

pub fn expected(case: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(
            repo()
                .join("tests/fixtures")
                .join(case)
                .join("expected.json"),
        )
        .unwrap(),
    )
    .unwrap()
}

pub const CASES: [&str; 11] = [
    "duplicate-notes",
    "version-correction",
    "conflict",
    "new-fact",
    "nothing-durable",
    "secret",
    "injection",
    "compacted",
    "codex-opted-in",
    "resumed",
    "sdk-driven",
];

fn copy(from: &Path, to: &Path, home: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (src, name) = (entry.path(), entry.file_name());
        if name == "expected.json" {
            continue;
        }
        let dst = to.join(&name);
        if src.is_dir() {
            fs::create_dir_all(&dst).unwrap();
            copy(&src, &dst, home);
        } else if name == ".dc-git-init" {
            let ok = Command::new("git")
                .args(["init", "-q"])
                .current_dir(to)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git init in {}", to.display());
        } else {
            let text = fs::read_to_string(&src).unwrap();
            fs::write(&dst, text.replace("@HOME@", &home.to_string_lossy())).unwrap();
        }
    }
}

pub fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() && !p.is_symlink() {
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            out.extend(walk(&p));
        } else if p.is_file() {
            out.push(p);
        }
    }
    out
}

/// Writes a Claude Code session file: an environment record with `cwd`, then per typed text
/// a human `user` record and an agent reply. Timestamps run from 09:00 on the fixture day.
pub fn session(home: &Home, folder: &str, uuid: &str, cwd: &str, typed: &[&str]) -> PathBuf {
    let ts = |m: usize| format!("2026-11-14T09:{m:02}:00.000Z");
    let mut lines = vec![
        serde_json::json!({"type":"attachment","cwd":cwd,"sessionId":uuid,"attachment":{"type":"environment"},"timestamp":ts(0)}),
    ];
    for (i, text) in typed.iter().enumerate() {
        lines.push(serde_json::json!({"type":"user","cwd":cwd,"origin":{"kind":"human"},"message":{"role":"user","content":text},"timestamp":ts(2 * i + 1)}));
        lines.push(serde_json::json!({"type":"assistant","cwd":cwd,"message":{"role":"assistant","content":[{"type":"text","text":"Noted."}]},"timestamp":ts(2 * i + 2)}));
    }
    let dir = home.dir.join(".claude/projects").join(folder);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{uuid}.jsonl"));
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&path, text).unwrap();
    home.age_files(2 * 3_600);
    path
}

/// Writes notes and a standard `MEMORY.md` into a project folder's store.
pub fn notes(home: &Home, folder: &str, notes: &[(&str, &str)]) -> PathBuf {
    let dir = home.store(folder);
    fs::create_dir_all(&dir).unwrap();
    let mut index = String::from("# Memory index\n");
    for (slug, body) in notes {
        fs::write(
            dir.join(format!("{slug}.md")),
            format!("---\nname: {slug}\ndescription: About {slug}\nmetadata:\n  type: project\n---\n\n{body}\n"),
        )
        .unwrap();
        index.push_str(&format!("- [{slug}]({slug}.md) \u{2014} About {slug}\n"));
    }
    if !notes.is_empty() {
        fs::write(dir.join("MEMORY.md"), index).unwrap();
    }
    home.age_files(2 * 3_600);
    dir
}

pub fn uuid(n: usize) -> String {
    format!("{n:08}-bbbb-4ccc-8ddd-{n:012}")
}
