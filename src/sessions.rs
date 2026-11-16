//! Session records from the agents' files or from pond, the one parser that turns them into
//! `Session` and `Transcript`, scopes and mapping, selection, and re-verify.

use crate::exec;
use crate::model::{
    Agent, Config, DAY, Finding, HOUR, Header, Message, Note, Role, Seen, Session, Source, Store,
    Time, Transcript,
};
use crate::screen;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const TOOL_CALL_CHARS: usize = 500;
const TOOL_RESULT_ENDS: usize = 1_000;
const TRANSCRIPT_CHARS: usize = 150_000;
const CUT: &str = "\n[...]\n";
const POND_PAGE: usize = 500;
const POND_TIMEOUT: Duration = Duration::from_secs(120);
const POND_SQL_TIMEOUT: &str = "60";
const HUMAN_ORIGINATORS: [&str; 2] = ["codex-tui", "Codex Desktop"];
const IDLE_FILES: i64 = HOUR;
/// Two pond syncs past the files' idle time.
const IDLE_POND: i64 = 70 * 60;
const WINDOW: i64 = 7 * DAY;
/// A scope waits while any of its sessions changed this recently.
const QUIET: i64 = 10 * 60;
const DUE_AFTER: i64 = 20 * HOUR;
const AUTO_DREAM_RECENT: i64 = 48 * HOUR;
pub const MAX_SCOPES: usize = 3;
pub const MAX_SESSIONS: usize = 12;
const MAX_STRIKES: usize = 3;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const VERIFY_NOTES: usize = 5;
const VERIFY_TERMS: usize = 8;
const HIT_CHARS: usize = 2_000;
const HITS_CHARS: usize = 30_000;
const HEAD_LINES: usize = 1_000;

#[derive(Debug)]
pub enum Error {
    /// A `pond` command failed, timed out or answered in an unexpected shape.
    SourceDown(String),
    /// A session file could not be read, or a record is not JSON.
    Parse(String),
}

/// A raw record and its record number: the file line, or pond's `source.line` or `ordinal`.
pub type Record = (u64, String);
/// A record parsed as JSON.
pub type Parsed = (u64, Value);

/// Top-level session files: `~/.claude/projects/<folder>/<uuid>.jsonl` and
/// `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, with each session's id.
pub fn files() -> Vec<(Agent, String, PathBuf)> {
    let home = std::env::home_dir().unwrap_or_default();
    let mut out = Vec::new();
    for project in entries(&home.join(".claude/projects")) {
        for path in entries(&project) {
            if path.extension().is_some_and(|e| e == "jsonl")
                && path.is_file()
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                out.push((Agent::Claude, stem.to_owned(), path.clone()));
            }
        }
    }
    for year in entries(&home.join(".codex/sessions")) {
        for day in entries(&year).iter().flat_map(|m| entries(m)) {
            for path in entries(&day) {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Some(stem) = name
                    .strip_prefix("rollout-")
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    && let Some(id) = stem.len().checked_sub(36).and_then(|i| stem.get(i..))
                    && path.is_file()
                {
                    out.push((Agent::Codex, id.to_owned(), path.clone()));
                }
            }
        }
    }
    out
}

/// The session's raw records in file order. `path` is used by `Source::Files` only.
pub fn read(
    source: &Source,
    agent: Agent,
    id: &str,
    path: Option<&Path>,
) -> Result<Vec<Record>, Error> {
    match source {
        Source::Files => {
            let path = path.ok_or_else(|| Error::Parse(format!("{id}: no session file")))?;
            let text = fs::read_to_string(path)
                .map_err(|e| Error::Parse(format!("{}: {e}", path.display())))?;
            Ok(text
                .split('\n')
                .zip(1..)
                .filter(|(line, _)| !line.trim().is_empty())
                .map(|(line, n)| (n, line.to_owned()))
                .collect())
        }
        Source::Pond(bin) => read_pond(bin, agent, id),
    }
}

/// `options.source.raw_record` of the session's `messages` rows, 500 per query. Codex rows
/// are numbered `ordinal + 1`, the file line (D22), after a `session_meta` record built from
/// the session row, which pond keeps out of `messages` (D24).
fn read_pond(bin: &Path, agent: Agent, id: &str) -> Result<Vec<Record>, Error> {
    // The id goes into SQL text, so only uuid characters pass.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(Error::Parse(format!("not a session id: {id}")));
    }
    let (number, shift) = match agent {
        Agent::Claude => ("json_get_int(options, 'source', 'line')", 0),
        Agent::Codex => (
            "json_get_int(options, 'source', 'raw_record', 'ordinal')",
            1,
        ),
    };
    #[derive(Deserialize)]
    struct Row {
        n: u64,
        r: String,
    }
    let mut out: Vec<Record> = Vec::new();
    if agent == Agent::Codex {
        out.extend(codex_meta(bin, id)?);
    }
    let mut after: i64 = -1;
    loop {
        let sql = format!(
            "SELECT {number} AS n, json_extract(options, '$.source.raw_record') AS r \
             FROM messages WHERE session_id = '{id}' AND {number} > {after} \
             ORDER BY n LIMIT {POND_PAGE}"
        );
        let mut rows = 0;
        for line in pond_sql(bin, &sql)? {
            let row: Row = serde_json::from_str(&line)
                .map_err(|e| Error::SourceDown(format!("pond sql: unexpected row: {e}")))?;
            after = row.n as i64;
            rows += 1;
            // A real ordinal-0 row would replace the synthetic record.
            if row.n + shift == 1 && out.first().is_some_and(|r| r.0 == 1) {
                out.remove(0);
            }
            out.push((row.n + shift, row.r));
        }
        if rows < POND_PAGE {
            return Ok(out);
        }
    }
}

/// The Codex session row as the `session_meta` record the parser reads: `source`,
/// `originator`, and `project` as `cwd`. None when pond has no such session.
fn codex_meta(bin: &Path, id: &str) -> Result<Option<Record>, Error> {
    #[derive(Deserialize)]
    struct Row {
        source: Option<String>,
        originator: Option<String>,
        project: Option<String>,
    }
    let sql = format!(
        "SELECT json_extract(options, '$.source.source') AS source, \
         json_get_string(options, 'source', 'originator') AS originator, project \
         FROM sessions WHERE session_id = '{id}' LIMIT 1"
    );
    let Some(line) = pond_sql(bin, &sql)?.into_iter().next() else {
        return Ok(None);
    };
    let row: Row = serde_json::from_str(&line)
        .map_err(|e| Error::SourceDown(format!("pond sql: unexpected session row: {e}")))?;
    let source = match row.source {
        Some(text) => serde_json::from_str(&text)
            .map_err(|e| Error::SourceDown(format!("pond sql: session source: {e}")))?,
        None => Value::Null,
    };
    let meta = serde_json::json!({
        "type": "session_meta",
        "payload": {"source": source, "originator": row.originator, "cwd": row.project},
    });
    Ok(Some((1, meta.to_string())))
}

/// A failed `pond` command as `SourceDown`. Its stdout may hold rows or hits, that is session
/// text, so only the status and stderr are kept, and those redacted.
fn down(e: exec::Error) -> Error {
    let head = e.0.split("\nstdout: ").next().unwrap_or_default();
    Error::SourceDown(screen::redact(head))
}

/// The ndjson lines of one `pond sql` query. Any failure is `SourceDown`.
pub(crate) fn pond_sql(bin: &Path, sql: &str) -> Result<Vec<String>, Error> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "sql",
        "--format",
        "ndjson",
        "--timeout",
        POND_SQL_TIMEOUT,
        sql,
    ]);
    let text = exec::run(&mut cmd, None, POND_TIMEOUT).map_err(down)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect())
}

/// One store as a scope: its `cwd`, and why it is skipped, if it is.
#[derive(Debug, Clone)]
pub struct Scope {
    pub store: Store,
    /// The start `cwd` of the newest session in the store's folder that has one.
    pub cwd: Option<String>,
    /// Not a scope at all: a worktree folder, or excluded in `config.json`.
    pub hidden: Option<&'static str>,
    /// A scope that does not run, with the reason `status` shows.
    pub skip: Option<&'static str>,
}

/// Every top-level session in the window that maps to a scope, parsed when it may be pending.
#[derive(Debug, Default)]
pub struct Listing {
    /// Parsed sessions, with `scope` and `changed` set.
    pub sessions: Vec<Session>,
    /// Refs of window sessions that map to no scope.
    pub unmapped: Vec<String>,
    /// Scopes that have a session that changed in the last 10 minutes.
    pub recent: HashSet<String>,
    /// Scopes with at least one mapped window session.
    pub mapped: HashSet<String>,
    /// Mapped window sessions not read in full: ref, scope, and why.
    pub skipped: Vec<(String, String, &'static str)>,
    /// Codex candidate sessions whose pond rows carry no `ordinal` (an older format). They are
    /// never read.
    pub codex_without_ordinal: u64,
}

/// The scopes of one tick, with `git rev-parse` answers cached per path.
#[derive(Default)]
pub struct Map {
    pub scopes: Vec<Scope>,
    git: HashMap<String, Option<(PathBuf, PathBuf)>>,
}

impl Map {
    /// The canonical git dir and common dir of `path` or its nearest existing ancestor. None
    /// when git fails, or when the repository root is the home folder.
    fn dirs(&mut self, path: &str) -> Option<(PathBuf, PathBuf)> {
        if let Some(hit) = self.git.get(path) {
            return hit.clone();
        }
        let found = Path::new(path)
            .ancestors()
            .find(|p| p.is_dir())
            .and_then(|dir| {
                let mut cmd = Command::new("git");
                cmd.arg("-C").arg(dir).args([
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-dir",
                    "--git-common-dir",
                ]);
                let out = exec::run(&mut cmd, None, GIT_TIMEOUT).ok()?;
                let mut lines = out.lines();
                let git_dir = fs::canonicalize(lines.next()?).ok()?;
                let common = fs::canonicalize(lines.next()?).ok()?;
                let home = std::env::home_dir().and_then(|h| fs::canonicalize(h).ok());
                (common.parent() != home.as_deref()).then_some((git_dir, common))
            });
        self.git.insert(path.to_owned(), found.clone());
        found
    }

    fn common(&mut self, path: &str) -> Option<PathBuf> {
        self.dirs(path).map(|(_, common)| common)
    }

    pub fn get(&self, id: &str) -> Option<&Scope> {
        self.scopes.iter().find(|s| s.store.id == id)
    }

    /// The scope a session maps to: the one whose folder holds it, else the one whose `cwd`
    /// has the same git common dir as the session's start `cwd` (the deepest `cwd` that
    /// contains it wins, else the first).
    fn map(&mut self, folder: Option<&str>, cwd: Option<&str>) -> Option<String> {
        let live: Vec<(String, Option<String>)> = self
            .scopes
            .iter()
            .filter(|s| s.hidden.is_none())
            .map(|s| (s.store.id.clone(), s.cwd.clone()))
            .collect();
        if let Some((id, _)) = folder.and_then(|f| live.iter().find(|(id, _)| id == f)) {
            return Some(id.clone());
        }
        let cwd = cwd?;
        let common = self.common(cwd)?;
        let matching: Vec<&(String, Option<String>)> = live
            .iter()
            .filter(|(_, c)| {
                c.as_deref()
                    .is_some_and(|c| self.common(c).as_ref() == Some(&common))
            })
            .collect();
        let depth = |s: &&(String, Option<String>)| {
            s.1.as_deref()
                .filter(|c| Path::new(cwd).starts_with(c))
                .map_or(0, str::len)
        };
        let best = matching
            .iter()
            .copied()
            .max_by_key(depth)
            .filter(|s| depth(s) > 0)
            .or(matching.first().copied());
        best.map(|(id, _)| id.clone())
    }
}

/// Each store as a scope, with its `cwd`, worktree and exclusion state, and skip reason.
pub fn scopes(
    source: &Source,
    stores: Vec<Store>,
    config: &Config,
    now: Time,
) -> Result<Map, Error> {
    let newest_cwd: HashMap<String, String> = match source {
        Source::Files => {
            let home = std::env::home_dir().unwrap_or_default();
            let mut out = HashMap::new();
            for store in &stores {
                let mut files: Vec<(Time, PathBuf)> =
                    entries(&home.join(".claude/projects").join(&store.id))
                        .into_iter()
                        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                        .map(|p| (modified(&p), p))
                        .collect();
                files.sort_by_key(|f| std::cmp::Reverse(f.0));
                if let Some(cwd) = files.iter().find_map(|(_, p)| head(Agent::Claude, p).0) {
                    out.insert(store.id.clone(), cwd);
                }
            }
            out
        }
        Source::Pond(bin) => {
            #[derive(Deserialize)]
            struct Row {
                dir: Option<String>,
                project: Option<String>,
            }
            let sql = "SELECT json_get_string(options, 'source', 'project_dir') AS dir, project, created_at \
                       FROM sessions WHERE source_agent = 'claude-code' \
                       AND (parent_session_id IS NULL OR parent_session_id = '') ORDER BY created_at";
            let mut out = HashMap::new();
            for line in pond_sql(bin, sql)? {
                let row: Row = serde_json::from_str(&line)
                    .map_err(|e| Error::SourceDown(format!("pond sql: unexpected row: {e}")))?;
                if let (Some(dir), Some(project)) = (row.dir, row.project) {
                    out.insert(dir, project);
                }
            }
            out
        }
    };
    let mut map = Map::default();
    for store in stores {
        let cwd = newest_cwd.get(&store.id).cloned();
        let worktree = cwd
            .as_deref()
            .and_then(|c| map.dirs(c))
            .is_some_and(|(g, c)| g != c);
        let hidden = if config.exclude.contains(&store.id) {
            Some("excluded")
        } else if worktree {
            Some("worktree folder")
        } else {
            None
        };
        let skip = match store.auto_dream {
            Some(true) => Some("auto-dream is on"),
            None if store
                .consolidated
                .is_some_and(|t| now.0 - t.0 < AUTO_DREAM_RECENT) =>
            {
                Some("auto-dream ran in the last 48 hours")
            }
            _ => None,
        };
        map.scopes.push(Scope {
            store,
            cwd,
            hidden,
            skip,
        });
    }
    Ok(map)
}

/// Window sessions mapped to scopes. A session is read in full only when it is idle, in a
/// live scope, and changed after its last consumed attempt.
pub fn list(
    source: &Source,
    now: Time,
    map: &mut Map,
    seen: &Seen,
    config: &Config,
) -> Result<Listing, Error> {
    let window_start = Time::parse(&config.init).unwrap_or(now).0 - WINDOW;
    struct Candidate {
        agent: Agent,
        id: String,
        path: Option<PathBuf>,
        folder: Option<String>,
        cwd: Option<String>,
        top: bool,
        changed: Time,
    }
    let mut candidates = Vec::new();
    let idle = match source {
        Source::Files => {
            for (agent, id, path) in files() {
                let changed = modified(&path);
                if changed.0 <= window_start {
                    continue;
                }
                let (cwd, top) = head(agent, &path);
                let folder = match agent {
                    Agent::Claude => path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|f| f.to_string_lossy().into_owned()),
                    Agent::Codex => None,
                };
                candidates.push(Candidate {
                    agent,
                    id,
                    path: Some(path),
                    folder,
                    cwd,
                    top,
                    changed,
                });
            }
            IDLE_FILES
        }
        Source::Pond(bin) => {
            #[derive(Deserialize)]
            struct Row {
                id: String,
                agent: String,
                project: Option<String>,
                dir: Option<String>,
                src: Option<String>,
                newest: String,
            }
            let sql = format!(
                "SELECT s.session_id AS id, s.source_agent AS agent, s.project AS project, \
                 json_get_string(s.options, 'source', 'project_dir') AS dir, \
                 json_extract(s.options, '$.source.source') AS src, max(m.timestamp) AS newest \
                 FROM sessions s JOIN messages m ON m.session_id = s.session_id \
                 WHERE (s.parent_session_id IS NULL OR s.parent_session_id = '') \
                 AND s.source_agent IN ('claude-code', 'codex-cli') AND m.timestamp > '{}' \
                 GROUP BY s.session_id, s.source_agent, s.project, \
                 json_get_string(s.options, 'source', 'project_dir'), json_extract(s.options, '$.source.source')",
                Time(window_start).rfc3339()
            );
            for line in pond_sql(bin, &sql)? {
                let row: Row = serde_json::from_str(&line)
                    .map_err(|e| Error::SourceDown(format!("pond sql: unexpected row: {e}")))?;
                let agent = match row.agent.as_str() {
                    "claude-code" => Agent::Claude,
                    _ => Agent::Codex,
                };
                let top = agent == Agent::Claude || row.src.is_some_and(|s| s.starts_with('"'));
                candidates.push(Candidate {
                    agent,
                    id: row.id,
                    path: None,
                    folder: row.dir.filter(|_| agent == Agent::Claude),
                    cwd: row.project,
                    top,
                    changed: Time::parse(&row.newest).unwrap_or_default(),
                });
            }
            IDLE_POND
        }
    };
    let mut out = Listing::default();
    let prefix = |a: Agent| {
        if a == Agent::Claude {
            "claude"
        } else {
            "codex"
        }
    };
    let mut check = Vec::new();
    for c in candidates.into_iter().filter(|c| c.top) {
        let reference = format!("{}:{}", prefix(c.agent), c.id);
        let Some(scope) = map.map(c.folder.as_deref(), c.cwd.as_deref()) else {
            out.unmapped.push(reference);
            continue;
        };
        out.mapped.insert(scope.clone());
        if now.0 - c.changed.0 < QUIET {
            out.recent.insert(scope.clone());
        }
        let consumed = seen.consumed.get(&reference);
        let skip = if map.get(&scope).is_none_or(|s| s.skip.is_some()) {
            Some("scope skipped")
        } else if now.0 - c.changed.0 < idle {
            Some("not idle")
        } else if c.agent == Agent::Codex && !config.cross_provider.contains(&scope) {
            Some("codex not opted in")
        } else if matches!(source, Source::Files) && consumed.is_some_and(|(_, t)| c.changed <= *t)
        {
            // Under the files source an unchanged file has no new records.
            Some("consumed, unchanged")
        } else {
            None
        };
        match skip {
            Some(why) => out.skipped.push((reference, scope, why)),
            None => check.push((c, reference, scope)),
        }
    }
    // Under pond, two cheap facts per session decide whether it is worth reading: a lower bound
    // of its typed messages, and its highest record number, which pond may raise late (Q14).
    let facts = match source {
        Source::Pond(bin) if !check.is_empty() => {
            let ids: Vec<&str> = check.iter().map(|(c, _, _)| c.id.as_str()).collect();
            pond_facts(bin, &ids)?
        }
        _ => HashMap::new(),
    };
    for (c, reference, scope) in check {
        let consumed = seen.consumed.get(&reference).map_or(0, |c| c.0);
        if let Some((typed, last, ordinal)) = facts.get(&c.id) {
            // ponytail: counted only among candidates that reach the facts query; a full count
            // costs minutes on a large pond.
            out.codex_without_ordinal += u64::from(c.agent == Agent::Codex && !ordinal);
            let why = if *typed < 2 {
                Some("fewer than 2 typed messages")
            } else if *last <= consumed {
                Some("consumed, unchanged")
            } else {
                None
            };
            if let Some(why) = why {
                out.skipped.push((reference, scope, why));
                continue;
            }
        }
        let records = match read(source, c.agent, &c.id, c.path.as_deref()).and_then(|r| parse(&r))
        {
            Ok(r) => r,
            Err(Error::SourceDown(e)) => return Err(Error::SourceDown(e)),
            Err(Error::Parse(_)) => {
                out.skipped.push((reference, scope, "unreadable"));
                continue;
            }
        };
        let mut s = session(c.agent, &c.id, c.path, &records);
        s.scope = Some(scope);
        s.changed = c.changed;
        out.sessions.push(s);
    }
    Ok(out)
}

/// Per session id: a lower bound of its typed messages (any record of human origin, or a
/// Codex `UserMessage` item), and its highest record number among record types the parser
/// renders (Claude Code `source.line`, Codex `ordinal + 1`, D22), and whether any row has an
/// `ordinal`.
fn pond_facts(bin: &Path, ids: &[&str]) -> Result<HashMap<String, (u64, u64, bool)>, Error> {
    #[derive(Deserialize)]
    struct Row {
        id: String,
        typed: Option<u64>,
        line: Option<u64>,
        ordinal: Option<u64>,
    }
    let ids: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|i| !i.is_empty() && i.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .collect();
    let raw = "json_get_string(m.options, 'source', 'raw_record'";
    let sql = format!(
        "SELECT m.session_id AS id, \
         sum(CASE WHEN {raw}, 'origin', 'kind') = 'human' \
         OR {raw}, 'attachment', 'origin', 'kind') = 'human' \
         OR {raw}, 'payload', 'item', 'type') = 'UserMessage' THEN 1 ELSE 0 END) AS typed, \
         max(CASE WHEN {raw}, 'type') IN ('user', 'assistant', 'attachment') \
         THEN json_get_int(m.options, 'source', 'line') END) AS line, \
         max(CASE WHEN {raw}, 'type') IN ('event_msg', 'response_item') \
         THEN json_get_int(m.options, 'source', 'raw_record', 'ordinal') END) AS ordinal \
         FROM messages m WHERE m.session_id IN ('{}') GROUP BY m.session_id",
        ids.join("', '")
    );
    let mut out = HashMap::new();
    for line in pond_sql(bin, &sql)? {
        let row: Row = serde_json::from_str(&line)
            .map_err(|e| Error::SourceDown(format!("pond sql: unexpected row: {e}")))?;
        let last = row.line.or(row.ordinal.map(|o| o + 1)).unwrap_or(0);
        out.insert(
            row.id,
            (row.typed.unwrap_or(0), last, row.ordinal.is_some()),
        );
    }
    Ok(out)
}

/// None when the session is pending, else the rule it fails.
pub fn pending(
    s: &Session,
    seen: &Seen,
    config: &Config,
    now: Time,
    source: &Source,
) -> Option<&'static str> {
    let idle = match source {
        Source::Files => IDLE_FILES,
        Source::Pond(_) => IDLE_POND,
    };
    let window_start = Time::parse(&config.init).unwrap_or(now).0 - WINDOW;
    let consumed = seen.consumed.get(&s.reference).map_or(0, |c| c.0);
    let scope = s.scope.as_deref().unwrap_or("");
    if !s.top_level {
        Some("not top-level")
    } else if s.human_typed < 2 {
        Some("not human-driven")
    } else if now.0 - s.changed.0 < idle {
        Some("not idle")
    } else if Time::parse(&s.last).is_none_or(|t| t.0 <= window_start) {
        Some("outside the window")
    } else if s.records <= consumed {
        Some("consumed")
    } else if s.agent == Agent::Codex && !config.cross_provider.iter().any(|c| c == scope) {
        Some("codex not opted in")
    } else if seen.strikes(&s.reference, s.records) >= MAX_STRIKES {
        Some("given up")
    } else {
        None
    }
}

/// The due scopes, oldest due clock first, at most 3, each with at most 12 pending sessions,
/// oldest first.
pub fn select<'a>(
    listing: &Listing,
    scopes: &'a [Scope],
    seen: &Seen,
    config: &Config,
    now: Time,
    source: &Source,
) -> Vec<(&'a Scope, Vec<Session>)> {
    let mut due: Vec<(&Scope, Vec<Session>)> = scopes
        .iter()
        .filter(|s| s.hidden.is_none() && s.skip.is_none() && !listing.recent.contains(&s.store.id))
        .filter(|s| {
            seen.due
                .get(&s.store.id)
                .is_none_or(|t| now.0 - t.0 >= DUE_AFTER)
        })
        .filter_map(|scope| {
            let mut pend: Vec<Session> = listing
                .sessions
                .iter()
                .filter(|s| s.scope.as_deref() == Some(scope.store.id.as_str()))
                .filter(|s| pending(s, seen, config, now, source).is_none())
                .cloned()
                .collect();
            pend.sort_by(|a, b| a.last.cmp(&b.last));
            pend.truncate(MAX_SESSIONS);
            (!pend.is_empty()).then_some((scope, pend))
        })
        .collect();
    due.sort_by_key(|(s, _)| seen.due.get(&s.store.id).copied());
    due.truncate(MAX_SCOPES);
    due
}

/// Re-verify (pond only): pond search hits for the 5 notes checked longest ago, kept when
/// they map to this scope and are new. Returns the hits and the slugs checked.
pub fn evidence(
    source: &Source,
    config: &Config,
    map: &mut Map,
    scope: &str,
    notes: &[Note],
    seen: &Seen,
    run: &[Session],
) -> Result<(Vec<Finding>, Vec<String>), Error> {
    let bin = match source {
        Source::Files => return Ok((Vec::new(), Vec::new())),
        Source::Pond(bin) => bin,
    };
    let Some(project) = map.get(scope).and_then(|s| s.cwd.clone()) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let id = &scope.to_owned();
    let mut order: Vec<&Note> = notes.iter().filter(|n| n.raw.is_none()).collect();
    order.sort_by_key(|n| {
        (
            seen.checked.get(&(id.clone(), n.slug.clone())).copied(),
            n.slug.clone(),
        )
    });
    order.truncate(VERIFY_NOTES);

    #[derive(Deserialize)]
    struct Found {
        sessions: Vec<Hit>,
    }
    #[derive(Deserialize)]
    struct Hit {
        session_id: String,
        project: Option<String>,
        source_agent: String,
        matches: Vec<Match>,
    }
    #[derive(Deserialize)]
    struct Match {
        text: String,
    }
    let mut hits: Vec<(&Note, Hit)> = Vec::new();
    for note in &order {
        // pond rejects an empty query and a date that is not YYYY-MM-DD (Q2). A note with no
        // search words is still counted as checked, so it does not stay first in line.
        let words = terms(note);
        if words.is_empty() {
            continue;
        }
        let from = note
            .sources
            .iter()
            .map(|s| s.1.as_str())
            // A real calendar date: pond rejects 2026-02-31 too.
            .filter(|d| Time::parse(&format!("{d}T00:00:00")).is_some_and(|t| t.date() == *d))
            .max()
            .unwrap_or(&note.file_date);
        let mut cmd = Command::new(bin);
        cmd.args([
            "search",
            &words.join(" "),
            "--project",
            &project,
            "--from-date",
            from,
        ])
        .args(["--sort-by", "recency", "--limit", "3", "--format", "json"]);
        let out = exec::run(&mut cmd, None, POND_TIMEOUT).map_err(down)?;
        let found: Found = serde_json::from_str(&out)
            .map_err(|e| Error::SourceDown(format!("pond search: unexpected output: {e}")))?;
        hits.extend(found.sessions.into_iter().map(|h| (*note, h)));
    }
    let checked = order.iter().map(|n| n.slug.clone()).collect();
    let ids: Vec<&str> = hits
        .iter()
        .map(|(_, h)| h.session_id.as_str())
        .filter(|i| !i.is_empty() && i.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .collect();
    if ids.is_empty() {
        return Ok((Vec::new(), checked));
    }
    #[derive(Deserialize)]
    struct Row {
        id: String,
        dir: Option<String>,
        newest: String,
    }
    let sql = format!(
        "SELECT s.session_id AS id, json_get_string(s.options, 'source', 'project_dir') AS dir, \
         max(m.timestamp) AS newest FROM sessions s JOIN messages m ON m.session_id = s.session_id \
         WHERE s.session_id IN ('{}') GROUP BY s.session_id, json_get_string(s.options, 'source', 'project_dir')",
        ids.join("', '")
    );
    let mut rows = HashMap::new();
    for line in pond_sql(bin, &sql)? {
        let row: Row = serde_json::from_str(&line)
            .map_err(|e| Error::SourceDown(format!("pond sql: unexpected row: {e}")))?;
        rows.insert(row.id, (row.dir, row.newest));
    }
    let in_run: HashSet<&str> = run.iter().map(|s| s.reference.as_str()).collect();
    let (mut out, mut budget) = (Vec::new(), HITS_CHARS);
    for (note, hit) in hits {
        let agent = match hit.source_agent.as_str() {
            "claude-code" => Agent::Claude,
            "codex-cli" if config.cross_provider.contains(id) => Agent::Codex,
            _ => continue,
        };
        let Some((dir, newest)) = rows.get(&hit.session_id) else {
            continue;
        };
        let reference = format!(
            "{}:{}",
            if agent == Agent::Claude {
                "claude"
            } else {
                "codex"
            },
            hit.session_id
        );
        let folder = dir.as_deref().filter(|_| agent == Agent::Claude);
        if map.map(folder, hit.project.as_deref()).as_deref() != Some(id.as_str())
            || in_run.contains(reference.as_str())
            || note.sources.iter().any(|(r, _)| *r == reference)
        {
            continue;
        }
        let text: Vec<&str> = hit.matches.iter().map(|m| m.text.as_str()).collect();
        let claim: String = screen::redact(&text.join("\n"))
            .chars()
            .take(HIT_CHARS)
            .collect();
        let size = claim.chars().count();
        if size > budget {
            break;
        }
        budget -= size;
        out.push(Finding {
            id: format!("{reference}#v{}", out.len() + 1),
            slug: Some(note.slug.clone()),
            kind: note.kind,
            claim,
            msg: None,
            session: reference,
            date: newest.get(..10).unwrap_or("").to_owned(),
        });
    }
    Ok((out, checked))
}

/// Up to 8 search words: the note's name words, then words of its code spans.
fn terms(note: &Note) -> Vec<String> {
    let spans = note.body.split('`').skip(1).step_by(2);
    let mut out: Vec<String> = Vec::new();
    for word in note
        .name
        .split_whitespace()
        .chain(spans.flat_map(str::split_whitespace))
    {
        let w: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || "-_.".contains(*c))
            .collect();
        let w = w.trim_matches(|c: char| !c.is_alphanumeric()).to_owned();
        if !w.is_empty()
            && !out.iter().any(|o| o.eq_ignore_ascii_case(&w))
            && out.len() < VERIFY_TERMS
        {
            out.push(w);
        }
    }
    out
}

/// The start `cwd` from the first lines of a session file, and for Codex whether its
/// `session_meta` marks it top-level.
fn head(agent: Agent, path: &Path) -> (Option<String>, bool) {
    let Ok(file) = fs::File::open(path) else {
        return (None, false);
    };
    let mut top = agent == Agent::Claude;
    for line in BufReader::new(file)
        .lines()
        .take(HEAD_LINES)
        .map_while(|l| l.ok())
    {
        let Ok(r) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if agent == Agent::Codex && r["type"] == "session_meta" {
            top = r["payload"]["source"].is_string();
        }
        if let Some(cwd) = cwd(agent, &r) {
            return (Some(cwd.to_owned()), top);
        }
    }
    (None, top)
}

fn modified(path: &Path) -> Time {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(Time::from_system)
        .unwrap_or_default()
}

/// Every record that is JSON. A line that is not (one cut by a crash) is left out; its number
/// stays unused, so the numbers of later records do not shift.
pub fn parse(records: &[Record]) -> Result<Vec<Parsed>, Error> {
    Ok(records
        .iter()
        .filter_map(|(n, raw)| serde_json::from_str(raw).ok().map(|v| (*n, v)))
        .collect())
}

/// How a rendered text is screened and cut.
#[derive(Clone, Copy)]
enum Cut {
    Full,
    Call,
    Result,
}

/// The facts the pending rules read. `records` counts only records the parser reads (D23):
/// rendered ones, typed ones, the one that gives the start `cwd`, and Codex `session_meta`.
pub fn session(agent: Agent, id: &str, path: Option<PathBuf>, records: &[Parsed]) -> Session {
    let prefix = match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    };
    let mut s = Session {
        reference: format!("{prefix}:{id}"),
        agent,
        path,
        start_cwd: None,
        records: 0,
        last: String::new(),
        human_typed: 0,
        top_level: agent == Agent::Claude,
        scope: None,
        changed: Time(0),
    };
    let (mut meta_seen, mut human_originator, mut typed) = (false, false, 0);
    for (n, r) in records {
        let mut read = false;
        if let Some(t) = r.get("timestamp").and_then(Value::as_str) {
            t.clone_into(&mut s.last);
        }
        if s.start_cwd.is_none() {
            s.start_cwd = cwd(agent, r).map(str::to_owned);
            read |= s.start_cwd.is_some();
        }
        items(agent, r, &mut |_, _, _| read = true);
        match agent {
            Agent::Claude => typed += usize::from(claude_typed(r).is_some()),
            Agent::Codex => {
                if r["type"] == "session_meta" && !meta_seen {
                    meta_seen = true;
                    read = true;
                    let p = &r["payload"];
                    s.top_level = p["source"].is_string()
                        && p.get("parent_thread_id").is_none_or(Value::is_null)
                        && p.get("forked_from_id").is_none_or(Value::is_null);
                    human_originator = p["originator"]
                        .as_str()
                        .is_some_and(|o| HUMAN_ORIGINATORS.contains(&o));
                }
                typed += usize::from(codex_typed(r).is_some());
            }
        }
        if read {
            s.records = s.records.max(*n);
        }
    }
    s.human_typed = match agent {
        Agent::Claude => typed,
        Agent::Codex if human_originator => typed,
        Agent::Codex => 0,
    };
    s
}

/// The session's messages after record `from`, redacted, cut, and capped.
pub fn transcript(s: &Session, records: &[Parsed], from: u64) -> Transcript {
    let mut messages = Vec::new();
    // Records past `s.records` arrived after listing; they are left for the run that consumes them.
    for (n, r) in records.iter().filter(|(n, _)| *n > from && *n <= s.records) {
        items(s.agent, r, &mut |role, cut, text: String| {
            let text = match cut {
                Cut::Full => screen::redact(&text),
                Cut::Call => tool_call(&text),
                Cut::Result => tool_result(&text),
            };
            if !text.is_empty() {
                messages.push(Message {
                    number: *n,
                    role,
                    text,
                });
            }
        });
    }
    Transcript {
        session: s.reference.clone(),
        header: Header {
            agent: s.agent,
            cwd: s.start_cwd.clone(),
            date: s.date().to_owned(),
        },
        messages: cap(messages),
    }
}

/// Reads the session from `source` and renders it from the record after `from`.
pub fn render(source: &Source, s: &Session, from: u64) -> Result<Transcript, Error> {
    let id = s.reference.split_once(':').map_or("", |(_, id)| id);
    let records = parse(&read(source, s.agent, id, s.path.as_deref())?)?;
    Ok(transcript(s, &records, from))
}

/// Every rendered item of one record, raw. Records that yield none are not rendered.
fn items(agent: Agent, r: &Value, push: &mut impl FnMut(Role, Cut, String)) {
    match agent {
        Agent::Claude => claude_items(r, push),
        Agent::Codex => codex_items(r, push),
    }
}

fn claude_items(r: &Value, push: &mut impl FnMut(Role, Cut, String)) {
    if let Some(content) = claude_typed(r) {
        return push(Role::User, Cut::Full, texts(content));
    }
    let content = &r["message"]["content"];
    match r["type"].as_str() {
        Some("assistant") => match content {
            Value::String(text) => push(Role::Agent, Cut::Full, text.clone()),
            Value::Array(blocks) => {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => {
                            push(Role::Agent, Cut::Full, texts(&b["text"]));
                        }
                        Some("tool_use") => {
                            push(
                                Role::Agent,
                                Cut::Call,
                                call_text(&b["name"], &b["input"].to_string()),
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        },
        Some("user") if r["isCompactSummary"] != true => {
            let text = match content {
                Value::Array(blocks) => blocks
                    .iter()
                    .map(|b| match b["type"].as_str() {
                        Some("tool_result") => texts(&b["content"]),
                        _ => texts(&b["text"]),
                    })
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
                other => texts(other),
            };
            push(Role::Tool, Cut::Result, text);
        }
        _ => {}
    }
}

fn codex_items(r: &Value, push: &mut impl FnMut(Role, Cut, String)) {
    if let Some(content) = codex_typed(r) {
        return push(Role::User, Cut::Full, texts(content));
    }
    if r["type"] != "response_item" {
        return;
    }
    let p = &r["payload"];
    match p["type"].as_str() {
        Some("message") if p["role"] == "assistant" => {
            push(Role::Agent, Cut::Full, texts(&p["content"]));
        }
        Some("function_call") => {
            // `arguments` is already JSON text.
            let input = p["arguments"]
                .as_str()
                .map_or_else(|| p["arguments"].to_string(), str::to_owned);
            push(Role::Agent, Cut::Call, call_text(&p["name"], &input));
        }
        Some("custom_tool_call") => {
            push(
                Role::Agent,
                Cut::Call,
                call_text(&p["name"], &p["input"].to_string()),
            );
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            push(Role::Tool, Cut::Result, texts(&p["output"]));
        }
        _ => {}
    }
}

/// The typed content: a human `user` record that is not meta or a compact summary, or a
/// `queued_command` attachment with human origin.
fn claude_typed(r: &Value) -> Option<&Value> {
    match r["type"].as_str()? {
        "user"
            if r["origin"]["kind"] == "human"
                && r["isMeta"] != true
                && r["isCompactSummary"] != true =>
        {
            Some(&r["message"]["content"])
        }
        "attachment"
            if r["attachment"]["type"] == "queued_command"
                && r["attachment"]["origin"]["kind"] == "human" =>
        {
            Some(&r["attachment"]["prompt"])
        }
        _ => None,
    }
}

/// The typed content: an `item_completed` event whose item is a `UserMessage`.
fn codex_typed(r: &Value) -> Option<&Value> {
    let p = &r["payload"];
    (r["type"] == "event_msg"
        && p["type"] == "item_completed"
        && p["item"]["type"] == "UserMessage")
        .then(|| &p["item"]["content"])
}

fn cwd(agent: Agent, r: &Value) -> Option<&str> {
    match agent {
        Agent::Claude => r["cwd"].as_str(),
        Agent::Codex => r["payload"]["cwd"]
            .as_str()
            .filter(|_| matches!(r["type"].as_str(), Some("session_meta" | "turn_context"))),
    }
}

/// A string, or the `text` fields of an array's items joined by newlines.
fn texts(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn call_text(name: &Value, input: &str) -> String {
    format!("{} {input}", name.as_str().unwrap_or(""))
}

/// Redacted, then cut to 500 characters.
fn tool_call(text: &str) -> String {
    let text = screen::redact(text);
    match text.char_indices().nth(TOOL_CALL_CHARS) {
        Some((i, _)) => text[..i].to_owned(),
        None => text,
    }
}

/// Redacted, then its first and last 1,000 characters.
fn tool_result(text: &str) -> String {
    let text = screen::redact(text);
    let count = text.chars().count();
    if count <= 2 * TOOL_RESULT_ENDS {
        return text;
    }
    let at = |k: usize| text.char_indices().nth(k).map_or(text.len(), |(i, _)| i);
    format!(
        "{}{CUT}{}",
        &text[..at(TOOL_RESULT_ENDS)],
        &text[at(count - TOOL_RESULT_ENDS)..]
    )
}

/// At most 150,000 characters: the first message, then the newest that fit.
fn cap(mut messages: Vec<Message>) -> Vec<Message> {
    let size = |m: &Message| m.text.chars().count();
    if messages.iter().map(size).sum::<usize>() <= TRANSCRIPT_CHARS {
        return messages;
    }
    let rest = messages.split_off(1);
    let mut first = messages;
    if let Some((i, _)) = first[0].text.char_indices().nth(TRANSCRIPT_CHARS) {
        first[0].text.truncate(i);
    }
    let mut budget = TRANSCRIPT_CHARS - size(&first[0]);
    let mut newest: Vec<Message> = rest
        .into_iter()
        .rev()
        .take_while(|m| {
            let fits = size(m) <= budget;
            budget -= if fits { size(m) } else { 0 };
            fits
        })
        .collect();
    newest.reverse();
    first.extend(newest);
    first
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    out.sort();
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    fn records(lines: &[Value]) -> Vec<Parsed> {
        lines
            .iter()
            .cloned()
            .zip(1..)
            .map(|(v, n)| (n, v))
            .collect()
    }

    fn roles(t: &Transcript) -> Vec<(u64, Role)> {
        t.messages.iter().map(|m| (m.number, m.role)).collect()
    }

    fn claude_fixture() -> Vec<Parsed> {
        let long = format!(
            "{}{}{}",
            "q".repeat(1_000),
            "m".repeat(500),
            "z".repeat(1_000)
        );
        let human = |text: &str| json!({"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":text},"timestamp":"2026-11-13T10:00:00.000Z"});
        records(&[
            json!({"type":"permission-mode","permissionMode":"default"}),
            json!({"type":"attachment","cwd":"/w/repo","attachment":{"type":"environment"}}),
            human("first ask with sk-abcdefghijklmnopqrstuvwxyz"),
            json!({"type":"assistant","cwd":"/other","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"on it"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"x".repeat(600)}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":long}]}}),
            json!({"type":"user","isMeta":true,"origin":{"kind":"human"},"message":{"content":"meta"}}),
            json!({"type":"user","isCompactSummary":true,"origin":{"kind":"human"},"message":{"content":"summary"}}),
            json!({"type":"attachment","attachment":{"type":"queued_command","origin":{"kind":"human"},"prompt":"queued ask"}}),
            json!({"type":"attachment","attachment":{"type":"queued_command","origin":{"kind":"task-notification"},"prompt":"done"}}),
            json!({"type":"user","origin":{"kind":"task-notification"},"message":{"content":[{"type":"text","text":"note"}]},"timestamp":"2026-11-14T09:30:00.000Z"}),
            json!({"type":"last-prompt"}),
        ])
    }

    #[test]
    fn claude_session_and_transcript() {
        let recs = claude_fixture();
        let s = session(Agent::Claude, "u1", None, &recs);
        assert_eq!(s.reference, "claude:u1");
        assert_eq!(s.start_cwd.as_deref(), Some("/w/repo"));
        // Record 13 is metadata, so it does not move `records` (D23).
        assert_eq!((s.records, s.human_typed, s.top_level), (12, 2, true));
        assert_eq!(s.date(), "2026-11-14");

        let t = transcript(&s, &recs, 0);
        assert_eq!(
            roles(&t),
            [
                (3, Role::User),
                (5, Role::Agent),
                (6, Role::Agent),
                (7, Role::Tool),
                (8, Role::Tool),
                (10, Role::User),
                (12, Role::Tool)
            ]
        );
        assert_eq!(t.messages[0].text, "first ask with [redacted]");
        assert_eq!(t.messages[2].text.chars().count(), TOOL_CALL_CHARS);
        assert!(t.messages[2].text.starts_with("Bash {\"command\""));
        let result = &t.messages[3].text;
        assert_eq!(result.chars().count(), 2 * TOOL_RESULT_ENDS + CUT.len());
        assert!(!result.contains('m') && result.starts_with('q') && result.ends_with('z'));
        assert_eq!(t.header.cwd.as_deref(), Some("/w/repo"));

        let resumed = transcript(&s, &recs, 7);
        assert_eq!(
            roles(&resumed),
            [(8, Role::Tool), (10, Role::User), (12, Role::Tool)]
        );
    }

    fn codex_fixture(source: Value, originator: &str) -> Vec<Parsed> {
        let typed = |text: &str| json!({"type":"event_msg","timestamp":"2026-11-14T08:00:00.000Z","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":text}]}}});
        let item = |p: Value| json!({"type":"response_item","timestamp":"2026-11-14T08:01:00.000Z","payload":p});
        records(&[
            json!({"type":"session_meta","timestamp":"2026-11-14T07:59:00.000Z","payload":{"cwd":"/w/codex","source":source,"originator":originator}}),
            json!({"type":"turn_context","payload":{"cwd":"/elsewhere"}}),
            item(
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}),
            ),
            item(
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"context"}]}),
            ),
            typed("fix the build"),
            item(json!({"type":"reasoning","summary":[]})),
            item(
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}),
            ),
            item(json!({"type":"function_call","name":"exec","arguments":"{\"cmd\":\"ls\"}"})),
            item(json!({"type":"function_call_output","output":"a b"})),
            item(json!({"type":"custom_tool_call","name":"apply_patch","input":"*** Begin"})),
            item(
                json!({"type":"custom_tool_call_output","output":[{"type":"input_text","text":"ok"}]}),
            ),
            typed("thanks"),
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[]}}}),
        ])
    }

    #[test]
    fn codex_session_and_transcript() {
        let recs = codex_fixture(json!("cli"), "codex-tui");
        let s = session(Agent::Codex, "c1", None, &recs);
        assert_eq!(
            (s.reference.as_str(), s.records, s.human_typed, s.top_level),
            ("codex:c1", 12, 2, true)
        );
        assert_eq!(s.start_cwd.as_deref(), Some("/w/codex"));
        let t = transcript(&s, &recs, 0);
        assert_eq!(
            roles(&t),
            [
                (5, Role::User),
                (7, Role::Agent),
                (8, Role::Agent),
                (9, Role::Tool),
                (10, Role::Agent),
                (11, Role::Tool),
                (12, Role::User)
            ]
        );
        let texts: Vec<&str> = t.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts[2], "exec {\"cmd\":\"ls\"}");
        assert_eq!(texts[3], "a b");
        assert_eq!(texts[4], "apply_patch \"*** Begin\"");
        assert_eq!(texts[5], "ok");

        let sub = session(
            Agent::Codex,
            "c2",
            None,
            &codex_fixture(json!({"subagent":{}}), "codex-tui"),
        );
        assert!(!sub.top_level);
        let sdk = session(
            Agent::Codex,
            "c3",
            None,
            &codex_fixture(json!("cli"), "acpx"),
        );
        assert_eq!(sdk.human_typed, 0);
    }

    #[test]
    fn cap_keeps_first_and_newest() {
        let m = |n, len| Message {
            number: n,
            role: Role::Agent,
            text: "x".repeat(len),
        };
        let capped = cap(vec![m(1, 10), m(2, 100_000), m(3, 90_000), m(4, 50_000)]);
        assert_eq!(
            capped.iter().map(|m| m.number).collect::<Vec<_>>(),
            [1, 3, 4]
        );
        let huge = cap(vec![m(1, 200_000), m(2, 5)]);
        assert_eq!(huge.len(), 1);
        assert_eq!(huge[0].text.len(), TRANSCRIPT_CHARS);
    }

    #[test]
    fn files_source_numbers_lines_and_skips_bad_json() {
        let dir = std::env::temp_dir().join(format!("dreamcatcher-files-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        fs::write(&path, "{\"a\":1}\n\n{\"cut\n{\"b\":2}\n").unwrap();
        let recs = read(&Source::Files, Agent::Claude, "s", Some(&path)).unwrap();
        assert_eq!(recs.iter().map(|r| r.0).collect::<Vec<_>>(), [1, 3, 4]);
        // F11: a line cut by a crash is left out and the numbers after it stay put.
        let parsed = parse(&recs).unwrap();
        assert_eq!(parsed.iter().map(|r| r.0).collect::<Vec<_>>(), [1, 4]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A stub `pond` that serves `total` message rows numbered from 1, honouring the `> N`
    /// bound and the page size, and one Codex session row.
    fn stub_pond(dir: &Path, total: u64, exit: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("pond");
        let script = r#"#!/bin/sh
[ EXIT -ne 0 ] && { echo down >&2; exit EXIT; }
case "$6" in *"FROM sessions"*)
  printf '{"source":"\\"cli\\"","originator":"codex-tui","project":"/w"}\n'; exit 0;; esac
after=$(printf '%s' "$6" | sed -n 's/.* > \(-*[0-9]*\) ORDER.*/\1/p')
n=$((after + 1)); [ $n -lt 1 ] && n=1; end=$((n + PAGE - 1)); [ $end -gt TOTAL ] && end=TOTAL
while [ $n -le $end ]; do
  printf '{"n":%d,"r":"{\\"type\\":\\"x\\",\\"i\\":%d}"}\n' $n $n
  n=$((n + 1))
done
"#
        .replace("EXIT", &exit.to_string())
        .replace("PAGE", &POND_PAGE.to_string())
        .replace("TOTAL", &total.to_string());
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn pond_source_pages_and_fails_as_source_down() {
        let dir = std::env::temp_dir().join(format!("dreamcatcher-pond-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let id = "0a1b-2c";
        let recs = read(
            &Source::Pond(stub_pond(&dir, 1_203, 0)),
            Agent::Claude,
            id,
            None,
        )
        .unwrap();
        assert_eq!(recs.len(), 1_203);
        assert_eq!(recs[1_202].0, 1_203);
        assert_eq!(parse(&recs).unwrap()[499].1, json!({"type":"x","i":500}));
        // Codex: the session row becomes record 1, ordinal k becomes record k + 1.
        let codex =
            parse(&read(&Source::Pond(stub_pond(&dir, 3, 0)), Agent::Codex, id, None).unwrap())
                .unwrap();
        assert_eq!(codex.iter().map(|r| r.0).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(
            codex[0].1,
            json!({"type":"session_meta","payload":{"source":"cli","originator":"codex-tui","cwd":"/w"}})
        );
        assert_eq!(codex[1].1["i"], 1);
        let s = session(Agent::Codex, id, None, &codex);
        assert!(s.top_level && s.start_cwd.as_deref() == Some("/w"));
        let down = read(&Source::Pond(stub_pond(&dir, 0, 3)), Agent::Codex, id, None);
        assert!(matches!(down, Err(Error::SourceDown(_))));
        let bad = read(
            &Source::Pond(dir.join("pond")),
            Agent::Claude,
            "x' OR 1=1",
            None,
        );
        assert!(matches!(bad, Err(Error::Parse(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    fn pending_fixture() -> (Session, Config, Map, Time) {
        let now = Time::parse("2026-11-14T12:00:00Z").unwrap();
        let s = Session {
            reference: "claude:a".into(),
            agent: Agent::Claude,
            path: None,
            start_cwd: None,
            records: 10,
            last: "2026-11-14T09:00:00.000Z".into(),
            human_typed: 2,
            top_level: true,
            scope: Some("p".into()),
            changed: Time(now.0 - 2 * HOUR),
        };
        let config = Config {
            init: "2026-11-10T00:00:00Z".into(),
            claude: "/x".into(),
            pond: None,
            source: "files".into(),
            exclude: vec![],
            cross_provider: vec![],
        };
        let store = Store {
            id: "p".into(),
            dir: "/x".into(),
            auto_dream: None,
            consolidated: None,
        };
        let map = Map {
            scopes: vec![Scope {
                store,
                cwd: None,
                hidden: None,
                skip: None,
            }],
            ..Map::default()
        };
        (s, config, map, now)
    }

    #[test]
    fn pending_rules() {
        let (s, config, _, now) = pending_fixture();
        let mut seen = Seen::default();
        let files = Source::Files;
        assert_eq!(pending(&s, &seen, &config, now, &files), None);
        let check =
            |s: &Session, seen: &Seen, source: &Source| pending(s, seen, &config, now, source);
        assert_eq!(
            check(
                &Session {
                    human_typed: 1,
                    ..s.clone()
                },
                &seen,
                &files
            ),
            Some("not human-driven")
        );
        assert_eq!(
            check(
                &Session {
                    top_level: false,
                    ..s.clone()
                },
                &seen,
                &files
            ),
            Some("not top-level")
        );
        let busy = Session {
            changed: Time(now.0 - 65 * 60),
            ..s.clone()
        };
        assert_eq!(check(&busy, &seen, &files), None);
        assert_eq!(
            check(&busy, &seen, &Source::Pond("/p".into())),
            Some("not idle")
        );
        let old = Session {
            last: "2026-11-02T09:00:00Z".into(),
            ..s.clone()
        };
        assert_eq!(check(&old, &seen, &files), Some("outside the window"));
        let codex = Session {
            agent: Agent::Codex,
            ..s.clone()
        };
        assert_eq!(check(&codex, &seen, &files), Some("codex not opted in"));
        seen.consumed.insert("claude:a".into(), (10, Time(0)));
        assert_eq!(check(&s, &seen, &files), Some("consumed"));
        seen.consumed.clear();
        seen.strikes.insert("claude:a".into(), vec![10, 10, 10]);
        assert_eq!(check(&s, &seen, &files), Some("given up"));
    }

    #[test]
    fn select_waits_for_the_due_clock_and_quiet_scopes() {
        let (s, config, map, now) = pending_fixture();
        let mut listing = Listing {
            sessions: vec![s],
            ..Listing::default()
        };
        let mut seen = Seen::default();
        let due = |l: &Listing, seen: &Seen| {
            select(l, &map.scopes, seen, &config, now, &Source::Files).len()
        };
        assert_eq!(due(&listing, &seen), 1);
        seen.due.insert("p".into(), Time(now.0 - 19 * HOUR));
        assert_eq!(due(&listing, &seen), 0);
        seen.due.insert("p".into(), Time(now.0 - 20 * HOUR));
        assert_eq!(due(&listing, &seen), 1);
        listing.recent.insert("p".into());
        assert_eq!(due(&listing, &seen), 0);
    }

    /// F16: `/w/mono/app-x` is not inside `/w/mono/app`.
    #[test]
    fn deepest_scope_compares_paths_not_strings() {
        let root = std::env::temp_dir().join(format!("dreamcatcher-mono-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["app", "app-x"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let root = fs::canonicalize(&root).unwrap();
        let scope = |id: &str, cwd: &Path| Scope {
            store: Store {
                id: id.into(),
                dir: "/x".into(),
                auto_dream: None,
                consolidated: None,
            },
            cwd: Some(cwd.to_string_lossy().into_owned()),
            hidden: None,
            skip: None,
        };
        let mut map = Map {
            scopes: vec![scope("app", &root.join("app")), scope("mono", &root)],
            ..Map::default()
        };
        let mapped = |map: &mut Map, d: &str| map.map(None, Some(&root.join(d).to_string_lossy()));
        assert_eq!(mapped(&mut map, "app-x").as_deref(), Some("mono"));
        assert_eq!(mapped(&mut map, "app").as_deref(), Some("app"));
        let _ = fs::remove_dir_all(&root);
    }

    /// F18: records appended after listing are left for the run that consumes them.
    #[test]
    fn transcript_stops_at_the_listed_records() {
        let recs = claude_fixture();
        let mut s = session(Agent::Claude, "u1", None, &recs);
        s.records = 7;
        let t = transcript(&s, &recs, 0);
        assert_eq!(t.messages.last().map(|m| m.number), Some(7));
    }

    /// Field names where two sessions differ, `path` aside.
    fn session_diff(a: &Session, b: &Session) -> Vec<&'static str> {
        [
            ("reference", a.reference == b.reference),
            ("agent", a.agent == b.agent),
            ("start_cwd", a.start_cwd == b.start_cwd),
            ("records", a.records == b.records),
            ("last", a.last == b.last),
            ("human_typed", a.human_typed == b.human_typed),
            ("top_level", a.top_level == b.top_level),
        ]
        .into_iter()
        .filter(|(_, same)| !same)
        .map(|(name, _)| name)
        .collect()
    }

    /// The first difference between two transcripts, as field, record number and role only.
    fn transcript_diff(a: &Transcript, b: &Transcript) -> Option<String> {
        if a.session != b.session {
            return Some("session".into());
        }
        if a.header != b.header {
            return Some("header".into());
        }
        for (i, (x, y)) in a.messages.iter().zip(&b.messages).enumerate() {
            for (field, same) in [
                ("number", x.number == y.number),
                ("role", x.role == y.role),
                ("text", x.text == y.text),
            ] {
                if !same {
                    return Some(format!(
                        "messages[{i}].{field}: files #{} {:?}, pond #{} {:?}",
                        x.number, x.role, y.number, y.role
                    ));
                }
            }
        }
        (a.messages.len() != b.messages.len()).then(|| {
            format!(
                "messages: files {}, pond {}",
                a.messages.len(),
                b.messages.len()
            )
        })
    }

    /// Panic messages can quote the text being processed, so tests on real data print
    /// only where a panic happened.
    pub(crate) fn quiet_panics() {
        std::panic::set_hook(Box::new(|info| {
            eprintln!("panic at {:?} (message withheld)", info.location());
        }));
    }

    /// AC24 against the owner's real files and pond, read-only. Prints refs, field names,
    /// record numbers, record types and counts, never record content.
    #[test]
    #[ignore]
    fn ac24_files_and_pond_agree() {
        use std::collections::BTreeMap;
        use std::time::SystemTime;
        quiet_panics();
        let pond = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|d| d.join("pond"))
            .find(|p| p.is_file())
            .expect("pond on PATH");
        let pond = Source::Pond(pond);
        let now = SystemTime::now();
        let age = |p: &Path| {
            fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
        };
        let mut all = files();
        all.sort_by_key(|(_, _, p)| age(p).unwrap_or(Duration::MAX));
        let (mut claude, mut codex, mut subagents) = (Vec::new(), Vec::new(), Vec::new());
        for (agent, id, path) in all {
            if age(&path).is_none_or(|a| a < Duration::from_secs(3_600)) {
                continue;
            }
            if agent == Agent::Claude && claude.len() == 20
                || agent == Agent::Codex && codex.len() == 5 && subagents.len() == 5
            {
                continue;
            }
            let Ok(recs) = read(&Source::Files, agent, &id, Some(&path)).and_then(|r| parse(&r))
            else {
                continue;
            };
            let s = session(agent, &id, Some(path), &recs);
            match agent {
                Agent::Claude if s.human_typed >= 2 => claude.push((s, recs)),
                Agent::Codex if s.top_level && s.human_typed >= 2 && codex.len() < 5 => {
                    codex.push((s, recs));
                }
                Agent::Codex if !s.top_level && subagents.len() < 5 => subagents.push(s),
                _ => {}
            }
            if claude.len() == 20 && codex.len() == 5 && subagents.len() == 5 {
                break;
            }
        }
        // Codex subagents and forks: both sources must agree they are not top-level.
        let mut top_level_disagree = 0;
        for fs_session in &subagents {
            let id = fs_session.reference.split_once(':').unwrap().1;
            let short = &fs_session.reference[..15];
            let pond_top = read(&pond, Agent::Codex, id, None)
                .and_then(|r| parse(&r))
                .map(|r| session(Agent::Codex, id, None, &r).top_level);
            let agree = matches!(pond_top, Ok(false));
            top_level_disagree += usize::from(!agree);
            println!(
                "{short}  subagent  files top_level=false  pond top_level={}",
                pond_top.map_or_else(|_| "read failed".into(), |t| t.to_string())
            );
        }

        let mut unequal = 0;
        println!(
            "AC24: {} Claude Code, {} Codex sessions",
            claude.len(),
            codex.len()
        );
        for (fs_session, fs_recs) in claude.iter().chain(&codex) {
            let agent = fs_session.agent;
            let id = fs_session.reference.split_once(':').unwrap().1;
            let short = &fs_session.reference[..fs_session.reference.len().min(15)];
            let pond_recs = match read(&pond, agent, id, None).and_then(|r| parse(&r)) {
                Ok(r) => r,
                Err(e) => {
                    unequal += 1;
                    let kind = if matches!(e, Error::SourceDown(_)) {
                        "source down"
                    } else {
                        "parse"
                    };
                    println!("{short}  NOT EQUAL  pond read failed: {kind}");
                    continue;
                }
            };
            let pond_session = session(agent, id, None, &pond_recs);
            let fields = session_diff(fs_session, &pond_session);
            let text = transcript_diff(
                &transcript(fs_session, fs_recs, 0),
                &transcript(&pond_session, &pond_recs, 0),
            );
            let equal = fields.is_empty() && text.is_none();
            unequal += usize::from(!equal);
            println!(
                "{short}  {}  session: [{}]  transcript: {}",
                if equal { "equal" } else { "NOT EQUAL" },
                fields.join(", "),
                text.as_deref().unwrap_or("equal")
            );

            let file: BTreeMap<u64, &Value> = fs_recs.iter().map(|(n, v)| (*n, v)).collect();
            let kind = |v: &Value| {
                let t = v["type"].as_str().unwrap_or("?").to_owned();
                match v["payload"]["type"].as_str() {
                    Some(p) => format!("{t}/{p}"),
                    None => t,
                }
            };
            match agent {
                Agent::Claude => {
                    // Q11: file records that pond lacks or holds differently.
                    let in_pond: BTreeMap<u64, &Value> =
                        pond_recs.iter().map(|(n, v)| (*n, v)).collect();
                    let mut missing: BTreeMap<String, Vec<u64>> = BTreeMap::new();
                    let mut changed: BTreeMap<String, Vec<u64>> = BTreeMap::new();
                    for (n, v) in &file {
                        match in_pond.get(n) {
                            None => missing.entry(kind(v)).or_default().push(*n),
                            Some(p) if p != v => changed.entry(kind(v)).or_default().push(*n),
                            Some(_) => {}
                        }
                    }
                    let extra = in_pond.keys().filter(|n| !file.contains_key(n)).count();
                    if !missing.is_empty() || !changed.is_empty() || extra > 0 {
                        println!(
                            "    Q11 missing in pond: {missing:?}  different: {changed:?}  pond-only: {extra}"
                        );
                    }
                }
                Agent::Codex => {
                    // D22: pond record k (ordinal + 1) must hold file line k.
                    let (mut same, mut off_by_one) = (0, 0);
                    for (k, v) in &pond_recs {
                        same += usize::from(file.get(k) == Some(&v));
                        off_by_one += usize::from(file.get(&(k + 1)) == Some(&v));
                    }
                    let first = file.values().next().map(|v| kind(v)).unwrap_or_default();
                    println!(
                        "    Q12 pond rows {}, file lines {}, pond number == line: {same}, pond number + 1 == line: {off_by_one}, file line 1 is {first}, same record in pond: {}",
                        pond_recs.len(),
                        file.len(),
                        pond_recs
                            .iter()
                            .any(|(_, v)| Some(v) == file.get(&1).copied())
                    );
                }
            }
        }
        assert_eq!(
            subagents.len(),
            5,
            "fewer than 5 idle Codex subagent sessions"
        );
        assert_eq!(
            top_level_disagree, 0,
            "{top_level_disagree} subagents look top-level"
        );
        assert_eq!(
            unequal, 0,
            "{unequal} sessions differ between files and pond"
        );
    }
}
