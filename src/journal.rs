//! The state folder: the run lock, crash recovery, the ledger and its reading rules,
//! `changes.json` snapshots and undo. Commit order is in `Journal::commit`.

use crate::exec::{Error, Result};
use crate::memory::{self, Written};
use crate::model::{AbortReason, DAY, Event, Plan, Planned, Seen, Time};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

const LEDGER: &str = "ledger.jsonl";
const CHANGES: &str = "changes.json";
const KEEP_RUNS: i64 = 30 * DAY;

/// `~/.local/share/ai-dreamcatcher`.
pub fn state_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join(".local/share/ai-dreamcatcher")
}

/// The state folder with mode 0700, and its `call/` folder.
pub fn ensure_state() -> Result<PathBuf> {
    let dir = state_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir.join("call"))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// The held run lock. Unlocks explicitly on drop.
pub struct Journal {
    dir: PathBuf,
    lock: File,
    /// The clock of this process, read once.
    now: Time,
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

/// The result of `Journal::commit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    Done,
    Aborted,
}

/// What `undo` did per scope: restored count and skipped slugs.
pub type Report = Vec<(String, usize, Vec<String>)>;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    store: PathBuf,
    plan: Plan,
}

/// Takes the OS lock on `lock`. None means another process holds it (busy).
pub fn lock(now: Time) -> Result<Option<Journal>> {
    let dir = ensure_state()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Journal {
            dir,
            lock: file,
            now,
        })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// The ledger by its reading rules, without the lock.
pub fn read() -> Seen {
    let text = fs::read_to_string(state_dir().join(LEDGER)).unwrap_or_default();
    // A line cut by a kill has no newline yet and is ignored.
    let complete = text.rfind('\n').map_or("", |i| &text[..i]);
    let events = complete
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .collect();
    seen(events)
}

fn seen(events: Vec<Event>) -> Seen {
    let key = |e: &Event| {
        (
            e.run.clone().unwrap_or_default(),
            e.scope.clone().unwrap_or_default(),
        )
    };
    let time = |e: &Event| Time::parse(&e.t).unwrap_or_default();
    let done: HashSet<(String, String)> =
        events.iter().filter(|e| e.ev == "done").map(key).collect();
    let aborted: HashSet<(String, String)> = events
        .iter()
        .filter(|e| e.ev == "aborted")
        .map(key)
        .collect();
    let was_aborted = |(run, scope): &(String, String)| {
        aborted.contains(&(run.clone(), scope.clone()))
            || aborted.contains(&(run.clone(), "*".into()))
    };
    let mut out = Seen::default();
    let mut attempts: HashMap<(String, String, String), Time> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        let session = e.session.clone().unwrap_or_default();
        match e.ev.as_str() {
            "attempt" => {
                let (run, scope) = key(e);
                attempts.insert((run.clone(), scope.clone(), session.clone()), time(e));
                let extracted = events[i + 1..].iter().any(|x| {
                    x.ev == "extracted"
                        && key(x) == (run.clone(), scope.clone())
                        && x.session == e.session
                });
                if !extracted && !was_aborted(&(run, scope)) {
                    out.strikes
                        .entry(session)
                        .or_default()
                        .push(e.records.unwrap_or(0));
                }
            }
            "retry" => {
                out.strikes.remove(&session);
            }
            "consumed" if done.contains(&key(e)) => {
                let (run, scope) = key(e);
                let at = attempts
                    .get(&(run, scope, session.clone()))
                    .copied()
                    .unwrap_or(time(e));
                let records = e.records.unwrap_or(0);
                if out.consumed.get(&session).is_none_or(|c| records >= c.0) {
                    out.consumed.insert(session, (records, at));
                }
            }
            "done" => {
                out.due.insert(key(e).1, time(e));
            }
            "aborted" if matches!(e.reason.as_deref(), Some("merge" | "cap" | "backend")) => {
                out.due.insert(key(e).1, time(e));
            }
            "verified" if done.contains(&key(e)) => {
                out.checked
                    .insert((key(e).1, e.slug.clone().unwrap_or_default()), time(e));
            }
            _ => {}
        }
    }
    out.events = events;
    out
}

pub fn reason(r: AbortReason) -> &'static str {
    match r {
        AbortReason::Stale => "stale",
        AbortReason::Crash => "crash",
        AbortReason::Merge => "merge",
        AbortReason::Cap => "cap",
        AbortReason::Backend => "backend",
        AbortReason::Source => "source",
    }
}

fn event(now: Time, ev: &str, run: &str, scope: &str) -> Event {
    Event {
        t: now.rfc3339(),
        ev: ev.into(),
        run: Some(run.into()),
        scope: Some(scope.into()),
        ..Event::default()
    }
}

/// Whether `runs/<run>` still holds a `changes.json`, so the run can be undone.
pub fn undoable(run: &str) -> bool {
    fs::read_dir(state_dir().join("runs").join(run))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| e.path().join(CHANGES).is_file())
}

impl Journal {
    /// Appends the events in one write and fsyncs the ledger.
    pub fn append(&self, events: &[Event]) -> Result<()> {
        let mut text = String::new();
        for e in events {
            text.push_str(&serde_json::to_string(e)?);
            text.push('\n');
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(LEDGER))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }

    pub fn attempt(&self, run: &str, scope: &str, session: &str, records: u64) -> Result<()> {
        let mut e = event(self.now, "attempt", run, scope);
        e.session = Some(session.into());
        e.records = Some(records);
        self.append(&[e])
    }

    pub fn extracted(&self, run: &str, scope: &str, session: &str) -> Result<()> {
        let mut e = event(self.now, "extracted", run, scope);
        e.session = Some(session.into());
        self.append(&[e])
    }

    pub fn abort(&self, run: &str, scope: &str, r: AbortReason) -> Result<()> {
        let mut e = event(self.now, "aborted", run, scope);
        e.reason = Some(reason(r).into());
        self.append(&[e])
    }

    pub fn retry(&self, session: &str) -> Result<()> {
        self.append(&[Event {
            t: self.now.rfc3339(),
            ev: "retry".into(),
            session: Some(session.into()),
            ..Event::default()
        }])
    }

    /// Cuts the ledger back to its last newline, then reverts every `changes.json` whose run
    /// and scope have neither `done`, `undone` nor a `crash` abort, removes temp files and
    /// appends `aborted` with `crash`. A `changes.json` with a change it could not revert (edited
    /// since) is kept, so `undo` can retry and nothing written is lost.
    pub fn recover(&self) -> Result<()> {
        let path = self.dir.join(LEDGER);
        if let Ok(bytes) = fs::read(&path)
            && !bytes.is_empty()
            && !bytes.ends_with(b"\n")
        {
            let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_len(keep as u64)?;
        }
        let seen = read();
        let closed: HashSet<(String, String)> = seen
            .events
            .iter()
            .filter(|e| e.ev == "done" || e.ev == "undone" || e.reason.as_deref() == Some("crash"))
            .map(|e| {
                (
                    e.run.clone().unwrap_or_default(),
                    e.scope.clone().unwrap_or_default(),
                )
            })
            .collect();
        for (run, scope, dir) in self.snapshots() {
            memory::remove_temps(&dir)?;
            if closed.contains(&(run.clone(), scope.clone())) {
                continue;
            }
            let file = dir.join(CHANGES);
            if let Ok(text) = fs::read_to_string(&file) {
                let snap: Snapshot = serde_json::from_str(&text)?;
                let mut skipped = Vec::new();
                for p in snap.plan.planned.iter().rev() {
                    if memory::revert(&snap.store, p)? == Written::Stale {
                        skipped.push(p.slug.as_str());
                    }
                }
                memory::remove_temps(&snap.store)?;
                if skipped.is_empty() {
                    fs::remove_file(&file)?;
                } else {
                    eprintln!(
                        "recover: run {run} scope {scope}: edited since, not reverted: {}; kept {}",
                        skipped.join(", "),
                        file.display()
                    );
                }
                self.abort(&run, &scope, AbortReason::Crash)?;
            }
        }
        Ok(())
    }

    /// `(run, scope, folder)` of every `runs/<run>/<scope>/` folder.
    fn snapshots(&self) -> Vec<(String, String, PathBuf)> {
        let list = |d: &Path| -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = fs::read_dir(d)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect();
            v.sort();
            v
        };
        let name = |p: &Path| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        list(&self.dir.join("runs"))
            .into_iter()
            .flat_map(|run| {
                list(&run)
                    .into_iter()
                    .map(move |scope| (name(&run), name(&scope), scope))
            })
            .collect()
    }

    /// Removes run folders older than 30 days. Their changes can no longer be undone.
    pub fn cleanup(&self) -> Result<()> {
        for entry in fs::read_dir(self.dir.join("runs")).into_iter().flatten() {
            let path = entry?.path();
            let old = path
                .file_name()
                .and_then(|n| Time::parse_run(&n.to_string_lossy()))
                .is_some_and(|t| self.now.0 - t.0 > KEEP_RUNS);
            if old {
                fs::remove_dir_all(&path)?;
            }
        }
        Ok(())
    }

    fn snapshot_path(&self, run: &str, scope: &str) -> PathBuf {
        self.dir.join("runs").join(run).join(scope).join(CHANGES)
    }

    fn write_snapshot(&self, run: &str, scope: &str, store: &Path, plan: &Plan) -> Result<PathBuf> {
        let path = self.snapshot_path(run, scope);
        let dir = path.parent().unwrap_or(&self.dir);
        fs::create_dir_all(dir)?;
        // New folders reach the disk only when their parents are synced too.
        let mut d = dir;
        while let Some(parent) = d.parent()
            && d != self.dir
        {
            memory::sync_dir(parent)?;
            d = parent;
        }
        let snap = Snapshot {
            store: store.to_owned(),
            plan: plan.clone(),
        };
        memory::replace(&path, serde_json::to_string(&snap)?.as_bytes())?;
        memory::sync_dir(dir)?;
        Ok(path)
    }

    /// The commit order: stale check, `changes.json`, notes and index lines, then every
    /// event ending with `consumed` lines and `done` in one write.
    pub fn commit(
        &self,
        run: &str,
        scope: &str,
        store: &Path,
        sessions: &[(String, u64)],
        plan: &Plan,
        checked: &[(String, usize)],
    ) -> Result<Commit> {
        for p in &plan.planned {
            if memory::stale(store, p)? {
                self.abort(run, scope, AbortReason::Stale)?;
                return Ok(Commit::Aborted);
            }
        }
        let snapshot = if plan.planned.is_empty() {
            None
        } else {
            Some(self.write_snapshot(run, scope, store, plan)?)
        };
        let mut written: Vec<&Planned> = Vec::new();
        for p in &plan.planned {
            // An I/O error ends the process here, so `recover` treats it as a crash.
            match memory::write(store, p)? {
                Written::Done | Written::Already => written.push(p),
                Written::Stale => {
                    for q in written.iter().rev() {
                        memory::revert(store, q)?;
                    }
                    if let Some(path) = &snapshot {
                        fs::remove_file(path)?;
                    }
                    self.abort(run, scope, AbortReason::Stale)?;
                    return Ok(Commit::Aborted);
                }
            }
        }
        let mut events = Vec::new();
        for p in &plan.planned {
            let mut e = event(self.now, "applied", run, scope);
            e.op = Some(p.op);
            e.slug = Some(p.slug.clone());
            e.description = Some(p.description.clone());
            e.sources = Some(p.sources.clone());
            events.push(e);
        }
        for d in &plan.dropped {
            let mut e = event(self.now, "dropped", run, scope);
            e.op = Some(d.op);
            e.slug = Some(d.slug.clone());
            e.reason = serde_json::to_value(d.reason)?.as_str().map(str::to_owned);
            events.push(e);
        }
        for (slug, hits) in checked {
            let mut e = event(self.now, "verified", run, scope);
            e.slug = Some(slug.clone());
            e.hits = Some(*hits);
            events.push(e);
        }
        for (session, records) in sessions {
            let mut e = event(self.now, "consumed", run, scope);
            e.session = Some(session.clone());
            e.records = Some(*records);
            events.push(e);
        }
        events.push(event(self.now, "done", run, scope));
        self.append(&events)?;
        Ok(Commit::Done)
    }

    /// Reverts every scope of `of` as run `run`, last change first, index line before note,
    /// skipping changes edited since. Its commit event is `undone`. A run that `recover` could
    /// not fully revert counts too.
    pub fn undo(&self, run: &str, of: &str) -> Result<Report> {
        let mut report = Vec::new();
        let seen = read();
        let committed: HashSet<String> = seen
            .events
            .iter()
            .filter(|e| e.ev == "done" || e.reason.as_deref() == Some("crash"))
            .filter(|e| e.run.as_deref() == Some(of))
            .filter_map(|e| e.scope.clone())
            .collect();
        for (r, scope, dir) in self.snapshots() {
            if r != of || !committed.contains(&scope) {
                continue;
            }
            let Ok(text) = fs::read_to_string(dir.join(CHANGES)) else {
                continue;
            };
            let snap: Snapshot = serde_json::from_str(&text)?;
            let inverse = Plan {
                planned: snap
                    .plan
                    .planned
                    .iter()
                    .rev()
                    .map(memory::inverse)
                    .collect(),
                dropped: Vec::new(),
            };
            self.write_snapshot(run, &scope, &snap.store, &inverse)?;
            let (mut restored, mut skipped) = (0, Vec::new());
            for p in snap.plan.planned.iter().rev() {
                match memory::revert(&snap.store, p)? {
                    Written::Done => restored += 1,
                    Written::Stale => skipped.push(p.slug.clone()),
                    Written::Already => {}
                }
            }
            let mut e = event(self.now, "undone", run, &scope);
            e.of = Some(of.into());
            e.restored = Some(restored);
            e.skipped = Some(skipped.clone());
            self.append(&[e])?;
            report.push((scope, restored, skipped));
        }
        if report.is_empty() {
            return Err(Error(format!("run {of} has no changes to undo")));
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(line: &str) -> Event {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn reading_rules() {
        let events: Vec<Event> = [
            // Run 1: s1 fails its extract (a strike), s2 succeeds; done.
            r#"{"t":"2026-11-10T10:00:00Z","ev":"attempt","run":"r1","scope":"p","session":"s1","records":5}"#,
            r#"{"t":"2026-11-10T10:00:00Z","ev":"attempt","run":"r1","scope":"p","session":"s2","records":9}"#,
            r#"{"t":"2026-11-10T10:01:00Z","ev":"extracted","run":"r1","scope":"p","session":"s2"}"#,
            r#"{"t":"2026-11-10T10:02:00Z","ev":"verified","run":"r1","scope":"p","slug":"n","hits":1}"#,
            r#"{"t":"2026-11-10T10:02:00Z","ev":"consumed","run":"r1","scope":"p","session":"s2","records":9}"#,
            r#"{"t":"2026-11-10T10:02:00Z","ev":"done","run":"r1","scope":"p"}"#,
            // Run 2: s1 fails again, then the whole tick aborts with source: no strike.
            r#"{"t":"2026-11-11T10:00:00Z","ev":"attempt","run":"r2","scope":"p","session":"s1","records":5}"#,
            r#"{"t":"2026-11-11T10:00:00Z","ev":"aborted","run":"r2","scope":"*","reason":"source"}"#,
            // Run 3: consumed without done does not count; a stale abort moves no due clock.
            r#"{"t":"2026-11-12T10:00:00Z","ev":"attempt","run":"r3","scope":"p","session":"s3","records":4}"#,
            r#"{"t":"2026-11-12T10:00:00Z","ev":"extracted","run":"r3","scope":"p","session":"s3"}"#,
            r#"{"t":"2026-11-12T10:00:00Z","ev":"consumed","run":"r3","scope":"p","session":"s3","records":4}"#,
            r#"{"t":"2026-11-12T10:00:00Z","ev":"aborted","run":"r3","scope":"p","reason":"stale"}"#,
            // Run 4 on scope q: merge failure moves the due clock.
            r#"{"t":"2026-11-13T10:00:00Z","ev":"aborted","run":"r4","scope":"q","reason":"merge"}"#,
        ]
        .iter()
        .map(|l| ev(l))
        .collect();
        let s = seen(events.clone());
        assert_eq!(s.strikes("s1", 5), 1);
        assert_eq!(s.strikes("s1", 6), 0, "records grew");
        assert_eq!(s.consumed.get("s2").map(|c| c.0), Some(9));
        assert!(!s.consumed.contains_key("s3"));
        assert_eq!(
            s.due.get("p"),
            Some(&Time::parse("2026-11-10T10:02:00Z").unwrap())
        );
        assert_eq!(
            s.due.get("q"),
            Some(&Time::parse("2026-11-13T10:00:00Z").unwrap())
        );
        assert!(s.checked.contains_key(&("p".to_owned(), "n".to_owned())));

        let mut more = events;
        more.push(ev(
            r#"{"t":"2026-11-13T11:00:00Z","ev":"retry","session":"s1"}"#,
        ));
        assert_eq!(seen(more).strikes("s1", 5), 0);
    }
}
