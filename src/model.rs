//! Types shared by every module, with only the conversions that belong to them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where session records come from. Matched exhaustively inside `sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The agents' own session files under `~/.claude` and `~/.codex`.
    Files,
    /// The `pond` CLI, started by this absolute path.
    Pond(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    User,
    Feedback,
    Project,
    Reference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Create,
    Update,
    Delete,
}

/// The closed set of `dropped` reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    Secret,
    Hidden,
    Noop,
    IndexFull,
    UnsafeDelete,
}

/// The closed set of `aborted` reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    Stale,
    Crash,
    Merge,
    Cap,
    Backend,
    Source,
}

/// A UTC instant in whole seconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Time(pub i64);

pub const HOUR: i64 = 3_600;
pub const DAY: i64 = 86_400;

impl Time {
    pub fn from_system(t: SystemTime) -> Time {
        Time(
            t.duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        )
    }

    /// `YYYY-MM-DDTHH:MM:SS` followed by anything (fraction, `Z`), read as UTC.
    pub fn parse(s: &str) -> Option<Time> {
        let b = s.as_bytes();
        let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
        if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
            return None;
        }
        let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
        let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
            return None;
        }
        Some(Time(
            days_from_civil(y, m, d) * DAY + hh * HOUR + mm * 60 + ss,
        ))
    }

    /// A run id, `YYYYMMDDTHHMMSSZ`.
    pub fn parse_run(s: &str) -> Option<Time> {
        let b = s.as_bytes();
        if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
            return None;
        }
        let iso = format!(
            "{}-{}-{}T{}:{}:{}",
            &s[0..4],
            &s[4..6],
            &s[6..8],
            &s[9..11],
            &s[11..13],
            &s[13..15]
        );
        Time::parse(&iso)
    }

    fn parts(self) -> (i64, i64, i64, i64, i64, i64) {
        let (days, secs) = (self.0.div_euclid(DAY), self.0.rem_euclid(DAY));
        let (y, m, d) = civil_from_days(days);
        (y, m, d, secs / HOUR, secs % HOUR / 60, secs % 60)
    }

    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub fn rfc3339(self) -> String {
        let (y, m, d, hh, mm, ss) = self.parts();
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }

    /// `YYYY-MM-DD`.
    pub fn date(self) -> String {
        let (y, m, d, ..) = self.parts();
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// `YYYYMMDDTHHMMSSZ`.
    pub fn run_id(self) -> String {
        let (y, m, d, hh, mm, ss) = self.parts();
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
    }
}

// Howard Hinnant's civil calendar algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `~/.config/ai-dreamcatcher/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub init: String,
    pub claude: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pond: Option<PathBuf>,
    /// `pond` or `files`.
    pub source: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub cross_provider: Vec<String>,
}

/// One project's memory folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// The project folder name, which is the scope id.
    pub id: String,
    /// `~/.claude/projects/<id>/memory`.
    pub dir: PathBuf,
    /// `autoDreamEnabled` in `~/.claude/settings.json`, None when absent.
    pub auto_dream: Option<bool>,
    /// File time of the store's `.consolidate-lock`, auto-dream's last consolidation.
    pub consolidated: Option<Time>,
}

/// One top-level session, as the shared parser reads it from either source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// `claude:<uuid>` or `codex:<uuid>`.
    pub reference: String,
    pub agent: Agent,
    /// The session file. None under `Source::Pond`.
    pub path: Option<PathBuf>,
    /// The `cwd` of the first record that has one.
    pub start_cwd: Option<String>,
    /// The highest number among the records the parser reads (D23).
    pub records: u64,
    /// RFC 3339 UTC time of the last record that has a timestamp, or empty.
    pub last: String,
    /// Typed messages that count as human: Claude Code by `origin.kind`, Codex `UserMessage`
    /// items only when the originator is `codex-tui` or `Codex Desktop`.
    pub human_typed: usize,
    /// Codex: the first `session_meta` has a string `source` and no parent or fork id.
    /// Claude Code: always true, because only files directly in a project folder are read.
    pub top_level: bool,
    /// The scope it maps to. Set by `sessions::list`.
    pub scope: Option<String>,
    /// File time, or under `Source::Pond` the newest `messages.timestamp`.
    pub changed: Time,
}

impl Session {
    /// UTC date of the last record, used in `Sources:`.
    pub fn date(&self) -> &str {
        self.last.get(..10).unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub agent: Agent,
    pub cwd: Option<String>,
    pub date: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Record number: line number in the file.
    pub number: u64,
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// The session's reference.
    pub session: String,
    pub header: Header,
    pub messages: Vec<Message>,
}

/// One memory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// File stem.
    pub slug: String,
    pub name: String,
    pub kind: Option<NoteKind>,
    pub description: String,
    /// Its line in `MEMORY.md`, if any.
    pub index_line: Option<String>,
    /// Text after the frontmatter, without a trailing `Sources:` line.
    pub body: String,
    /// `(ref, date)` pairs of the `Sources:` line.
    pub sources: Vec<(String, String)>,
    /// UTC date of the file's modification time.
    pub file_date: String,
    /// Frontmatter lines as read, without the `---` markers. None when the file has none.
    pub front: Option<Vec<String>>,
    /// The file's bytes, only when they are not valid UTF-8. Such a note is read-only.
    pub raw: Option<Vec<u8>>,
}

/// `MEMORY.md` split on `\n`, so joining the lines gives the file back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub lines: Vec<String>,
    /// The file's bytes, only when they are not valid UTF-8. Such an index is read-only.
    pub raw: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// `<ref>#<n>`, or `<ref>#v<n>` for a re-verify hit.
    pub id: String,
    pub slug: Option<String>,
    pub kind: Option<NoteKind>,
    pub claim: String,
    /// The message number it rests on. None for a hit.
    pub msg: Option<u64>,
    pub session: String,
    pub date: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Change {
    pub op: Op,
    pub slug: String,
    pub kind: Option<NoteKind>,
    pub name: String,
    pub description: String,
    pub body: String,
    pub findings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEdit {
    pub old: Option<String>,
    pub new: Option<String>,
    /// Line number and the line before it.
    pub anchor: (usize, Option<String>),
    pub created_index: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Planned {
    pub op: Op,
    pub slug: String,
    pub description: String,
    pub sources: Vec<String>,
    pub file: FileEdit,
    pub index: IndexEdit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    pub op: Op,
    pub slug: String,
    pub reason: DropReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Plan {
    pub planned: Vec<Planned>,
    pub dropped: Vec<Dropped>,
}

/// One `ledger.jsonl` line. Which fields are set depends on `ev`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Event {
    pub t: String,
    pub ev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hits: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<Op>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<String>>,
}

/// What the ledger says, by its reading rules. Built by `journal::read`.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Every event, in order.
    pub events: Vec<Event>,
    /// Per session ref: the last consumed `records` and the time of its `attempt`.
    pub consumed: HashMap<String, (u64, Time)>,
    /// Per session ref: the `records` of each strike since its last `retry`.
    pub strikes: HashMap<String, Vec<u64>>,
    /// Per scope: the due clock (last `done`, or `aborted` with `merge`, `cap` or `backend`).
    pub due: HashMap<String, Time>,
    /// Per scope and slug: the last committed `verified` time.
    pub checked: HashMap<(String, String), Time>,
}

impl Seen {
    /// Strikes that still count for a session whose `records` is now `records`.
    pub fn strikes(&self, reference: &str, records: u64) -> usize {
        self.strikes
            .get(reference)
            .map_or(0, |s| s.iter().filter(|r| **r >= records).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_round_trips() {
        let t = Time::parse("2026-11-14T10:17:03.123Z").unwrap();
        assert_eq!(t.rfc3339(), "2026-11-14T10:17:03Z");
        assert_eq!(t.run_id(), "20261114T101703Z");
        assert_eq!(Time::parse_run("20261114T101703Z"), Some(t));
        assert_eq!(Time(0).date(), "1970-01-01");
        assert_eq!(Time(1_700_000_000).date(), "2023-11-14");
        assert_eq!(Time(951_782_400).date(), "2000-02-29");
        assert_eq!(Time::parse("2000-02-29T00:00:00Z"), Some(Time(951_782_400)));
        assert_eq!(Time::parse("garbage"), None);
    }
}
