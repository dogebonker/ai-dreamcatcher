//! Claude Code memory stores: discovery, reading, planning changes and writing them.

use crate::exec::Result;
use crate::model::{
    Change, DropReason, Dropped, FileEdit, Finding, Index, IndexEdit, Note, NoteKind, Op, Plan,
    Planned, Store, Time,
};
use crate::screen;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::Path;

pub const INDEX: &str = "MEMORY.md";
/// Claude Code loads at most this much of `MEMORY.md`.
pub const INDEX_LINES: usize = 200;
pub const INDEX_BYTES: usize = 25_000;
pub const MAX_CHANGES: usize = 10;
pub const MAX_DELETES: usize = 2;
const DESCRIPTION_CHARS: usize = 150;
const BODY_CHARS: usize = 4_000;
const SLUG_CHARS: usize = 64;
const SOURCES_KEPT: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notes {
    /// Sorted by slug.
    pub notes: Vec<Note>,
    /// None when the store has no `MEMORY.md`.
    pub index: Option<Index>,
}

/// Every `~/.claude/projects/*/memory` folder, sorted by id.
pub fn discover() -> Vec<Store> {
    let home = std::env::home_dir().unwrap_or_default();
    let claude = home.join(".claude");
    let auto_dream = fs::read(claude.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("autoDreamEnabled")?.as_bool());
    let Ok(entries) = fs::read_dir(claude.join("projects")) else {
        return Vec::new();
    };
    let mut stores: Vec<Store> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            let dir = e.path().join("memory");
            dir.is_dir().then(|| Store {
                id: e.file_name().to_string_lossy().into_owned(),
                consolidated: fs::metadata(dir.join(".consolidate-lock"))
                    .and_then(|m| m.modified())
                    .ok()
                    .map(Time::from_system),
                dir,
                auto_dream,
            })
        })
        .collect();
    stores.sort_by(|a, b| a.id.cmp(&b.id));
    stores
}

/// Reads every `*.md` note and `MEMORY.md` of the store.
pub fn open(store: &Store) -> Result<Notes> {
    let index = match fs::read(store.dir.join(INDEX)) {
        Ok(bytes) => {
            let mut index = parse_index(bytes.clone());
            // A symlinked index is read-only: rename would replace the link, not its target.
            if is_link(&store.dir.join(INDEX)) {
                index.raw = Some(bytes);
            }
            Some(index)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut notes = Vec::new();
    for entry in fs::read_dir(&store.dir)? {
        let path = entry?.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let Some(slug) = name
            .as_deref()
            .filter(|n| *n != INDEX && !n.starts_with('.'))
            .and_then(|n| n.strip_suffix(".md"))
        else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let file_date = Time::from_system(fs::metadata(&path)?.modified()?).date();
        let bytes = fs::read(&path)?;
        let mut note = parse_note(slug, bytes.clone(), file_date, index.as_ref());
        if is_link(&path) {
            note.raw = Some(bytes);
        }
        notes.push(note);
    }
    notes.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(Notes { notes, index })
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Why a model output cannot be planned. The scope run's try fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// The output changes more than 10 notes or deletes more than 2.
    pub cap: bool,
    pub reason: String,
}

/// Whether a `Planned` reached the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Done,
    Stale,
    /// `revert` only: the change was already back, nothing was written.
    Already,
}

/// Characters of note text a model call gets for this store.
pub fn size(notes: &Notes) -> usize {
    notes
        .notes
        .iter()
        .filter(|n| n.raw.is_none())
        .map(|n| n.name.len() + n.description.len() + n.body.len())
        .sum()
}

/// Decides in code what the merge output may change. Pure.
pub fn plan(
    notes: &Notes,
    changes: &[Change],
    findings: &[Finding],
) -> std::result::Result<Plan, Invalid> {
    let invalid = |cap, reason: String| Err(Invalid { cap, reason });
    let deletes = changes.iter().filter(|c| c.op == Op::Delete).count();
    if changes.len() > MAX_CHANGES || deletes > MAX_DELETES {
        return invalid(
            true,
            format!("{} changes, {deletes} deletes", changes.len()),
        );
    }
    let by_id: HashMap<&str, &Finding> = findings.iter().map(|f| (f.id.as_str(), f)).collect();
    let by_slug: HashMap<&str, &Note> = notes.notes.iter().map(|n| (n.slug.as_str(), n)).collect();
    let mut seen = HashSet::new();
    for c in changes {
        let slug = c.slug.as_str();
        if !seen.insert(slug) {
            return invalid(false, format!("two changes for {slug}"));
        }
        if let Some(id) = c
            .findings
            .iter()
            .find(|id| !by_id.contains_key(id.as_str()))
        {
            return invalid(false, format!("{slug}: unknown finding {id}"));
        }
        let note = by_slug.get(slug);
        match c.op {
            Op::Create => {
                // macOS folders ignore case, so `memory` is `MEMORY.md` and `deploy` is `Deploy.md`.
                let lower = slug.to_ascii_lowercase();
                if lower == "memory"
                    || notes
                        .notes
                        .iter()
                        .any(|n| n.slug.to_ascii_lowercase() == lower)
                {
                    return invalid(false, format!("create of existing {slug}"));
                }
                if !is_slug(slug) || screen::secret(slug).is_some() {
                    return invalid(false, format!("bad slug {slug:?}"));
                }
                if c.kind.is_none() {
                    return invalid(false, format!("{slug}: create without kind"));
                }
            }
            Op::Update | Op::Delete => {
                // A read-only note (not UTF-8, or a symlink) is never changed; the model never saw it.
                if note.is_none_or(|n| n.raw.is_some()) {
                    return invalid(false, format!("{:?} of missing {slug}", c.op));
                }
            }
        }
        if c.op == Op::Delete {
            continue;
        }
        // An update may rest on no finding: one that only consolidates notes (a duplicate).
        if c.op == Op::Create && c.findings.is_empty() {
            return invalid(false, format!("{slug}: create without findings"));
        }
        if one_line(&c.name).is_empty()
            || one_line(&c.description).chars().count() > DESCRIPTION_CHARS
        {
            return invalid(
                false,
                format!("{slug}: name empty or description over {DESCRIPTION_CHARS}"),
            );
        }
        let limit = note.map_or(BODY_CHARS, |n| BODY_CHARS.max(n.body.chars().count()));
        if c.body.trim().is_empty() || c.body.chars().count() > limit {
            return invalid(
                false,
                format!("{slug}: body empty or over {limit} characters"),
            );
        }
    }

    // An update that cites no finding is a dedup only: it keeps its kind, and it is paired with
    // a delete in the same output whose note already holds every line it adds. Its text then
    // comes from a note that cited a session, and it takes that note's sources (G3).
    let mut paired: HashMap<&str, &Note> = HashMap::new();
    for c in changes
        .iter()
        .filter(|c| c.op == Op::Update && c.findings.is_empty())
    {
        let slug = c.slug.as_str();
        let note = by_slug[slug];
        if c.kind.is_some_and(|k| Some(k) != note.kind) {
            return invalid(false, format!("{slug}: uncited update changes kind"));
        }
        // A strict merge: every body line of the kept note stays.
        let new: HashSet<&str> = c.body.lines().map(str::trim).collect();
        if note
            .body
            .lines()
            .map(str::trim)
            .any(|l| !l.is_empty() && !new.contains(l))
        {
            return invalid(
                false,
                format!("{slug}: uncited update drops lines of its own note"),
            );
        }
        let (_, _, added) = render(Some(note), c, &[], false);
        let holds = |d: &Note| {
            let text: HashSet<&str> = d
                .body
                .lines()
                .map(str::trim)
                .chain([d.name.as_str(), d.description.as_str()])
                .collect();
            added
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .all(|l| text.contains(l))
        };
        let partner = changes
            .iter()
            .filter(|d| d.op == Op::Delete)
            .map(|d| by_slug[d.slug.as_str()])
            .find(|d| !paired.values().any(|p| p.slug == d.slug) && holds(d));
        match partner {
            Some(d) => {
                paired.insert(slug, d);
            }
            None => {
                return invalid(
                    false,
                    format!("{slug}: uncited update is not a dedup of a deleted note"),
                );
            }
        }
    }

    let mut out = Plan::default();
    let index_lines = notes.index.as_ref().map_or(Vec::new(), |i| i.lines.clone());
    let count = |l: &[String]| l.len() - usize::from(l.last().is_some_and(String::is_empty));
    let mut lines = count(&index_lines);
    let mut bytes = index_lines.join("\n").len();
    for c in changes.iter().filter(|c| c.op != Op::Delete) {
        let note = by_slug.get(c.slug.as_str()).copied();
        let cited: Vec<&Finding> = c.findings.iter().map(|id| by_id[id.as_str()]).collect();
        // An uncited update carries the sources of the one note it absorbs.
        let deleted: Vec<(String, String)> = paired
            .get(c.slug.as_str())
            .map_or(Vec::new(), |d| d.sources.clone());
        let pairs: Vec<(String, String)> = cited
            .iter()
            .map(|f| (f.session.clone(), f.date.clone()))
            .chain(deleted)
            .collect();
        let (before, after, added) = render(note, c, &pairs, paired.contains_key(c.slug.as_str()));
        let drop = |reason| Dropped {
            op: c.op,
            slug: c.slug.clone(),
            reason,
        };
        if screen::secret(&added).is_some() {
            out.dropped.push(drop(DropReason::Secret));
            continue;
        }
        if screen::hidden(&added) {
            out.dropped.push(drop(DropReason::Hidden));
            continue;
        }
        if before.as_deref() == Some(after.as_str()) {
            out.dropped.push(drop(DropReason::Noop));
            continue;
        }
        let new_line = index_line(&one_line(&c.name), &c.slug, &one_line(&c.description));
        let no_edit = IndexEdit {
            old: None,
            new: None,
            anchor: (0, None),
            created_index: false,
        };
        let index = match note {
            // An identical line is already there: nothing to add, and nothing to repeat.
            _ if index_lines.contains(&new_line) => no_edit,
            None => {
                let pos = index_lines
                    .iter()
                    .rposition(|l| l.starts_with("- ["))
                    .map_or(count(&index_lines), |p| p + 1);
                IndexEdit {
                    old: None,
                    new: Some(new_line.clone()),
                    anchor: (pos, pos.checked_sub(1).map(|p| index_lines[p].clone())),
                    created_index: notes.index.is_none(),
                }
            }
            Some(n) => match &n.index_line {
                Some(old)
                    if one_line(&c.name) != n.name || one_line(&c.description) != n.description =>
                {
                    IndexEdit {
                        old: Some(old.clone()),
                        new: Some(new_line.clone()),
                        anchor: (0, None),
                        created_index: false,
                    }
                }
                _ => no_edit,
            },
        };
        let grows = match (&index.old, &index.new) {
            (None, Some(new)) => Some((1, new.len() + 1)),
            (Some(old), Some(new)) if new.len() > old.len() => Some((0, new.len() - old.len())),
            _ => None,
        };
        let readonly = notes.index.as_ref().is_some_and(|i| i.raw.is_some()) && index.new.is_some();
        if let Some((l, b)) = grows {
            if readonly || lines + l > INDEX_LINES || bytes + b > INDEX_BYTES {
                out.dropped.push(drop(DropReason::IndexFull));
                continue;
            }
            lines += l;
            bytes += b;
        }
        out.planned.push(Planned {
            op: c.op,
            slug: c.slug.clone(),
            description: one_line(&c.description),
            sources: c.findings.clone(),
            file: FileEdit {
                name: format!("{}.md", c.slug),
                before,
                after: Some(after),
            },
            index,
        });
    }
    // A delete must take the note's line with it. When lines link to the note but none is its
    // unique own line, or the index is read-only, the line would dangle: drop the delete.
    // It must also leave a trail.
    let target = |slug: &str| format!("{slug}.md");
    // A delete with no trail removes only itself; no other change depended on it.
    let mut no_trail: HashSet<&str> = HashSet::new();
    for c in changes.iter().filter(|c| c.op == Op::Delete) {
        let linked = index_lines
            .iter()
            .any(|l| first_link(l) == Some(target(&c.slug).as_str()));
        // A trail (G3, AC4): the delete cites a finding about this note, or an update in the
        // same output keeps every line of it.
        let gone = by_slug[c.slug.as_str()];
        let about = c
            .findings
            .iter()
            .any(|id| by_id[id.as_str()].slug.as_deref() == Some(c.slug.as_str()));
        let absorbed = changes.iter().filter(|u| u.op == Op::Update).any(|u| {
            let kept: HashSet<&str> = u.body.lines().map(str::trim).collect();
            gone.body
                .lines()
                .map(str::trim)
                .all(|l| l.is_empty() || kept.contains(l))
        });
        let (dangling, trailless) = (linked && gone.index_line.is_none(), !(about || absorbed));
        if dangling || trailless {
            out.dropped.push(Dropped {
                op: c.op,
                slug: c.slug.clone(),
                reason: DropReason::UnsafeDelete,
            });
            // A dangling line still cascades; a missing trail alone does not.
            if !dangling {
                no_trail.insert(c.slug.as_str());
            }
        }
    }
    for c in changes.iter().filter(|c| c.op == Op::Delete) {
        if out
            .dropped
            .iter()
            .any(|d| d.op == Op::Delete && d.slug == c.slug)
        {
            continue;
        }
        // A `noop` keeps a note that already says it all, so it loses nothing a delete needs.
        let cascades = |d: &Dropped| {
            d.reason != DropReason::Noop
                && !(d.op == Op::Delete && no_trail.contains(d.slug.as_str()))
        };
        if out.dropped.iter().any(cascades) {
            out.dropped.push(Dropped {
                op: c.op,
                slug: c.slug.clone(),
                reason: DropReason::UnsafeDelete,
            });
            continue;
        }
        let n = by_slug[c.slug.as_str()];
        let pos = n
            .index_line
            .as_ref()
            .and_then(|l| index_lines.iter().position(|x| x == l));
        out.planned.push(Planned {
            op: Op::Delete,
            slug: c.slug.clone(),
            description: n.description.clone(),
            sources: c.findings.clone(),
            file: FileEdit {
                name: format!("{}.md", c.slug),
                before: Some(String::from_utf8_lossy(&serialize(n)).into_owned()),
                after: None,
            },
            index: IndexEdit {
                old: n.index_line.clone(),
                new: None,
                anchor: pos.map_or((0, None), |p| {
                    (p, p.checked_sub(1).map(|q| index_lines[q].clone()))
                }),
                created_index: false,
            },
        });
    }
    // The absorbed note must really go; otherwise the update's text and sources are unearned,
    // so the update is dropped with its delete and the rest of the output stands.
    for (u, d) in &paired {
        let delete_planned = out
            .planned
            .iter()
            .any(|p| p.op == Op::Delete && p.slug == d.slug);
        let i = out
            .planned
            .iter()
            .position(|p| p.op == Op::Update && p.slug == *u);
        if let (false, Some(i)) = (delete_planned, i) {
            out.planned.remove(i);
            out.dropped.push(Dropped {
                op: Op::Update,
                slug: (*u).to_owned(),
                reason: DropReason::UnsafeDelete,
            });
        }
    }
    Ok(out)
}

/// The note's text before and after a create or update, and the text the change adds.
/// `absorbs`: an update that cites no finding takes the deleted notes' sources even when its
/// body stays the same, so a duplicate's citations survive its delete.
fn render(
    note: Option<&Note>,
    c: &Change,
    cited: &[(String, String)],
    absorbs: bool,
) -> (Option<String>, String, String) {
    let (name, description) = (one_line(&c.name), one_line(&c.description));
    let quoted = |v: &str| serde_json::to_string(v).unwrap_or_default();
    let kind = c.kind.or(note.and_then(|n| n.kind)).map(kind_name);
    let mut added = Vec::new();
    let Some(n) = note else {
        let mut sources = Vec::new();
        merge_sources(&mut sources, cited);
        let new = Note {
            slug: c.slug.clone(),
            name: name.clone(),
            kind: c.kind,
            description: description.clone(),
            index_line: None,
            body: format!("\n{}\n", c.body.trim()),
            sources,
            file_date: String::new(),
            front: Some(vec![
                format!("name: {}", quoted(&name)),
                format!("description: {}", quoted(&description)),
                "metadata:".into(),
                format!("  type: {}", kind.unwrap_or("project")),
            ]),
            raw: None,
        };
        let after = String::from_utf8_lossy(&serialize(&new)).into_owned();
        return (None, after.clone(), after);
    };
    let before = String::from_utf8_lossy(&serialize(n)).into_owned();
    let mut new = n.clone();
    let mut front = n.front.clone().unwrap_or_default();
    let set = |key: &str, value: String, front: &mut Vec<String>| match front
        .iter()
        .position(|l| l.split_once(':').is_some_and(|(k, _)| k == key))
    {
        Some(i) => front[i] = format!("{key}: {value}"),
        None => front.insert(0, format!("{key}: {value}")),
    };
    if name != n.name {
        set("name", quoted(&name), &mut front);
        added.push(name.clone());
    }
    if description != n.description {
        set("description", quoted(&description), &mut front);
        added.push(description.clone());
    }
    if let Some(k) = c.kind.filter(|k| Some(*k) != n.kind) {
        let k = kind_name(k);
        let meta = front.iter().position(|l| l == "metadata:");
        let typed = |l: &String| l.trim_start().starts_with("type:");
        match front.iter().position(typed) {
            Some(i) => {
                let indent = &front[i][..front[i].len() - front[i].trim_start().len()];
                front[i] = format!("{indent}type: {k}");
            }
            None => match meta {
                Some(m) => front.insert(m + 1, format!("  type: {k}")),
                None => front.extend(["metadata:".into(), format!("  type: {k}")]),
            },
        }
    }
    new.front = Some(front);
    if c.body.trim() != n.body.trim() {
        let lead = if n.body.starts_with('\n') { "\n" } else { "" };
        new.body = format!("{lead}{}\n", c.body.trim());
        let old: HashSet<&str> = before.lines().collect();
        added.extend(
            new.body
                .lines()
                .filter(|l| !old.contains(l))
                .map(str::to_owned),
        );
    }
    if c.body.trim() != n.body.trim() || absorbs {
        merge_sources(&mut new.sources, cited);
    }
    let after = String::from_utf8_lossy(&serialize(&new)).into_owned();
    (Some(before), after, added.join("\n"))
}

/// Adds the cited sessions with their dates and keeps the 5 newest, newest first.
fn merge_sources(sources: &mut Vec<(String, String)>, cited: &[(String, String)]) {
    for (session, date) in cited {
        match sources.iter_mut().find(|(r, _)| r == session) {
            Some(s) if s.1 < *date => s.1.clone_from(date),
            Some(_) => {}
            None => sources.push((session.clone(), date.clone())),
        }
    }
    sources.sort_by(|a, b| b.1.cmp(&a.1));
    sources.truncate(SOURCES_KEPT);
}

fn kind_name(k: NoteKind) -> &'static str {
    match k {
        NoteKind::User => "user",
        NoteKind::Feedback => "feedback",
        NoteKind::Project => "project",
        NoteKind::Reference => "reference",
    }
}

pub fn index_line(name: &str, slug: &str, description: &str) -> String {
    format!("- [{name}]({slug}.md) \u{2014} {description}")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_slug(s: &str) -> bool {
    (1..=SLUG_CHARS).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The same change undone: before and after swapped, old and new index lines swapped.
pub fn inverse(p: &Planned) -> Planned {
    let mut q = p.clone();
    q.file.before.clone_from(&p.file.after);
    q.file.after.clone_from(&p.file.before);
    q.index.old.clone_from(&p.index.new);
    q.index.new.clone_from(&p.index.old);
    q
}

/// Where one half of a change stands on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Half {
    Before,
    After,
    /// Neither: someone else changed it.
    Neither,
}

fn note_half(dir: &Path, f: &FileEdit) -> Result<Half> {
    let current = read_opt(&dir.join(&f.name))?;
    let is = |t: &Option<String>| match (t, &current) {
        (Some(t), Some(c)) => t.as_bytes() == c.as_slice(),
        (None, None) => true,
        _ => false,
    };
    Ok(if is(&f.before) {
        Half::Before
    } else if is(&f.after) {
        Half::After
    } else {
        Half::Neither
    })
}

fn index_half(dir: &Path, e: &IndexEdit) -> Result<Half> {
    let text = read_opt(&dir.join(INDEX))?.map(|b| String::from_utf8_lossy(&b).into_owned());
    let lines: Vec<&str> = text
        .as_deref()
        .map_or(Vec::new(), |t| t.split('\n').collect());
    let count = |l: &Option<String>| {
        l.as_deref()
            .map(|l| lines.iter().filter(|x| **x == l).count())
    };
    Ok(match (count(&e.old), count(&e.new)) {
        // No index edit: nothing to check, nothing to write.
        (None, None) | (Some(1), None | Some(0)) | (None, Some(0)) => Half::Before,
        (Some(0), None | Some(1)) | (None, Some(1)) => Half::After,
        _ => Half::Neither,
    })
}

/// True when the note or its index line is not where the change starts from.
pub fn stale(dir: &Path, p: &Planned) -> Result<bool> {
    Ok(note_half(dir, &p.file)? != Half::Before || index_half(dir, &p.index)? != Half::Before)
}

/// Writes one change unless it is stale: the note, then its index line. Fsyncs the folder.
pub fn write(dir: &Path, p: &Planned) -> Result<Written> {
    if stale(dir, p)? {
        return Ok(Written::Stale);
    }
    write_note(dir, &p.file)?;
    write_index(dir, &p.index)?;
    sync_dir(dir)?;
    Ok(Written::Done)
}

/// Takes one change back, index line first, for undo and recover. The note half and the index
/// half are judged separately: a half still in its written state is reverted, a half already
/// back is left alone (a crash may have written only one). When either half is in neither
/// state, someone edited it since, and the whole change is skipped.
pub fn revert(dir: &Path, p: &Planned) -> Result<Written> {
    let q = inverse(p);
    let (note, index) = (note_half(dir, &q.file)?, index_half(dir, &q.index)?);
    if note == Half::Neither || index == Half::Neither {
        return Ok(Written::Stale);
    }
    let index_due = index == Half::Before && (q.index.old.is_some() || q.index.new.is_some());
    if !index_due && note != Half::Before {
        return Ok(Written::Already);
    }
    if index_due {
        write_index(dir, &q.index)?;
    }
    if note == Half::Before {
        write_note(dir, &q.file)?;
    }
    sync_dir(dir)?;
    Ok(Written::Done)
}

fn write_note(dir: &Path, f: &FileEdit) -> Result<()> {
    let path = dir.join(&f.name);
    match &f.after {
        Some(text) => replace(&path, text.as_bytes()),
        None => match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        },
    }
}

/// A fresh read of `MEMORY.md` with one line patched, written through a temp file. The
/// file's last newline, or its lack of one, is kept.
fn write_index(dir: &Path, e: &IndexEdit) -> Result<()> {
    let path = dir.join(INDEX);
    let Some(bytes) = read_opt(&path)? else {
        return match &e.new {
            Some(line) if e.old.is_none() => replace(&path, format!("{line}\n").as_bytes()),
            _ => Ok(()),
        };
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if let (Some(old), None) = (&e.old, &e.new)
        && e.created_index
        && text == format!("{old}\n")
    {
        fs::remove_file(&path)?;
        return Ok(());
    }
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    match (&e.old, &e.new) {
        (Some(old), new) => {
            let Some(i) = lines.iter().position(|l| l == old) else {
                return Ok(());
            };
            match new {
                Some(new) => lines[i].clone_from(new),
                None => {
                    lines.remove(i);
                }
            }
        }
        (None, Some(new)) => {
            // At the recorded line number when the line before it is still the anchor's
            // previous line; else after the first line equal to it; else at the number.
            let end = lines.len() - usize::from(lines.last().is_some_and(String::is_empty));
            let (n, prev) = (&e.anchor.0, &e.anchor.1);
            let before = n.checked_sub(1).map(|i| lines.get(i).cloned());
            let at = if *n <= end && before.unwrap_or(None) == *prev {
                *n
            } else {
                prev.as_ref()
                    .and_then(|p| lines.iter().position(|l| l == p))
                    .map_or((*n).min(end), |p| p + 1)
            };
            lines.insert(at, new.clone());
        }
        (None, None) => return Ok(()),
    }
    replace(&path, lines.join("\n").as_bytes())
}

/// `.<name>.dc-tmp`, fsync, rename.
pub fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.dc-tmp"));
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn read_opt(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Removes `.*.dc-tmp` files a killed write left behind.
pub fn remove_temps(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with('.') && name.ends_with(".dc-tmp") {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// The note's file bytes, rebuilt from its parts.
pub fn serialize(note: &Note) -> Vec<u8> {
    if let Some(raw) = &note.raw {
        return raw.clone();
    }
    let mut out = String::new();
    if let Some(front) = &note.front {
        out.push_str("---\n");
        for line in front {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("---\n");
    }
    out.push_str(&note.body);
    if !note.sources.is_empty() {
        out.push_str(&sources_line(&note.sources));
        out.push('\n');
    }
    out.into_bytes()
}

pub fn serialize_index(index: &Index) -> Vec<u8> {
    match &index.raw {
        Some(raw) => raw.clone(),
        None => index.lines.join("\n").into_bytes(),
    }
}

fn parse_index(bytes: Vec<u8>) -> Index {
    let (text, raw) = text_of(bytes);
    Index {
        lines: text.split('\n').map(str::to_owned).collect(),
        raw,
    }
}

fn parse_note(slug: &str, bytes: Vec<u8>, file_date: String, index: Option<&Index>) -> Note {
    let (text, raw) = text_of(bytes);
    let (front, rest) = split_front(&text);
    let (body, sources) = split_sources(rest);
    let (mut name, mut description, mut kind) = (String::new(), String::new(), None);
    let mut in_metadata = false;
    for line in front.iter().flatten() {
        let top = !line.starts_with([' ', '\t']);
        let Some((key, value)) = line.split_once(':') else {
            in_metadata &= !top;
            continue;
        };
        match (top, key.trim()) {
            (true, "name") => name = scalar(value),
            (true, "description") => description = scalar(value),
            (true, "type") => kind = note_kind(&scalar(value)),
            (false, "type") if in_metadata => kind = note_kind(&scalar(value)),
            _ => {}
        }
        if top {
            in_metadata = key.trim() == "metadata";
        }
    }
    Note {
        slug: slug.to_owned(),
        name,
        kind,
        description,
        index_line: index.and_then(|i| own_line(i, slug)),
        body: body.to_owned(),
        sources,
        file_date,
        front,
        raw,
    }
}

/// The note's own index line: the one line whose first link targets `<slug>.md`. None when
/// there is no such line, more than one, or the index is read-only, so a change never patches
/// a line it cannot find again exactly once.
fn own_line(index: &Index, slug: &str) -> Option<String> {
    if index.raw.is_some() {
        return None;
    }
    let target = format!("{slug}.md");
    let mut found = index
        .lines
        .iter()
        .filter(|l| first_link(l) == Some(target.as_str()));
    let line = found.next()?;
    found.next().is_none().then(|| line.clone())
}

/// The target of the line's first `](...)` link, whatever the bullet or emphasis before it.
fn first_link(line: &str) -> Option<&str> {
    let start = line.find("](")? + 2;
    let end = start + line[start..].find(')')?;
    Some(&line[start..end])
}

/// The text, and the original bytes when they are not valid UTF-8.
fn text_of(bytes: Vec<u8>) -> (String, Option<Vec<u8>>) {
    match String::from_utf8(bytes) {
        Ok(text) => (text, None),
        Err(e) => (
            String::from_utf8_lossy(e.as_bytes()).into_owned(),
            Some(e.into_bytes()),
        ),
    }
}

/// Frontmatter lines and the rest. A file without a closed `---` block has no frontmatter.
fn split_front(text: &str) -> (Option<Vec<String>>, &str) {
    let Some(after) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let mut lines = Vec::new();
    let mut at = 0;
    while let Some(n) = after[at..].find('\n') {
        let line = &after[at..at + n];
        if line == "---" {
            return (Some(lines), &after[at + n + 1..]);
        }
        lines.push(line.to_owned());
        at += n + 1;
    }
    (None, text)
}

/// The body and the pairs of a final `Sources:` line. Only a line in the exact written form
/// counts, so serialize gives it back unchanged; any other last line stays in the body.
fn split_sources(rest: &str) -> (&str, Vec<(String, String)>) {
    let Some(text) = rest.strip_suffix('\n') else {
        return (rest, Vec::new());
    };
    let start = text.rfind('\n').map_or(0, |n| n + 1);
    let line = &text[start..];
    let Some(list) = line.strip_prefix("Sources: ") else {
        return (rest, Vec::new());
    };
    let pairs: Option<Vec<(String, String)>> = list
        .split(", ")
        .map(|p| {
            let (r, d) = p.split_once(' ')?;
            (!r.is_empty() && !d.is_empty() && !d.contains(' '))
                .then(|| (r.to_owned(), d.to_owned()))
        })
        .collect();
    match pairs {
        Some(pairs) if sources_line(&pairs) == line => (&rest[..start], pairs),
        _ => (rest, Vec::new()),
    }
}

fn sources_line(pairs: &[(String, String)]) -> String {
    let list: Vec<String> = pairs.iter().map(|(r, d)| format!("{r} {d}")).collect();
    format!("Sources: {}", list.join(", "))
}

/// A frontmatter value: JSON-quoted, single-quoted or plain.
fn scalar(value: &str) -> String {
    let v = value.trim();
    if v.starts_with('"') {
        return serde_json::from_str(v).unwrap_or_else(|_| v.to_owned());
    }
    match v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        Some(inner) => inner.replace("''", "'"),
        None => v.to_owned(),
    }
}

fn note_kind(s: &str) -> Option<NoteKind> {
    match s {
        "user" => Some(NoteKind::User),
        "feedback" => Some(NoteKind::Feedback),
        "project" => Some(NoteKind::Project),
        "reference" => Some(NoteKind::Reference),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn store(name: &str, files: &[(&str, &[u8])]) -> Store {
        let dir = std::env::temp_dir().join(format!("dreamcatcher-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in files {
            fs::write(dir.join(name), bytes).unwrap();
        }
        Store {
            id: "fixture".into(),
            dir,
            auto_dream: None,
            consolidated: None,
        }
    }

    #[test]
    fn parses_fields_and_round_trips() {
        let notes: [(&str, &[u8]); 9] = [
            (
                "meta.md",
                b"---\nname: Deploy steps\ndescription: \"How we \\\"ship\\\"\"\nmetadata:\n  type: project\n  modified: 2026-11-01\n---\n\nBody line.\nSources: claude:abc#3 2026-11-14, codex:def#9 2026-11-13\n",
            ),
            ("top.md", b"---\nname: 'It''s top'\ntype: feedback\n---\nbody\n"),
            ("bare.md", b"no frontmatter at all"),
            ("open.md", b"---\nname: never closed\n"),
            ("empty-front.md", b"---\n---\n"),
            ("odd-sources.md", b"---\nname: x\n---\nSources: not a list\n"),
            ("latin1.md", b"---\nname: caf\xe9\n---\n"),
            (".consolidate-lock", b""),
            (
                "MEMORY.md",
                b"# Index\n- [Deploy steps](meta.md) \xe2\x80\x94 how\nfree text\n\n- [Top](top.md) \xe2\x80\x94 t",
            ),
        ];
        let s = store("round-trip", &notes);
        let opened = open(&s).unwrap();
        let slugs: Vec<&str> = opened.notes.iter().map(|n| n.slug.as_str()).collect();
        assert_eq!(
            slugs,
            [
                "bare",
                "empty-front",
                "latin1",
                "meta",
                "odd-sources",
                "open",
                "top"
            ]
        );
        for note in &opened.notes {
            let disk = fs::read(s.dir.join(format!("{}.md", note.slug))).unwrap();
            assert_eq!(serialize(note), disk, "{}", note.slug);
        }
        let index = opened.index.as_ref().unwrap();
        assert_eq!(serialize_index(index), fs::read(s.dir.join(INDEX)).unwrap());

        let meta = &opened.notes[3];
        assert_eq!(meta.name, "Deploy steps");
        assert_eq!(meta.description, "How we \"ship\"");
        assert_eq!(meta.kind, Some(NoteKind::Project));
        assert_eq!(meta.body, "\nBody line.\n");
        assert_eq!(meta.sources[1], ("codex:def#9".into(), "2026-11-13".into()));
        assert!(
            meta.index_line
                .as_deref()
                .unwrap()
                .starts_with("- [Deploy steps]")
        );
        let top = &opened.notes[6];
        assert_eq!(
            (top.name.as_str(), top.kind),
            ("It's top", Some(NoteKind::Feedback))
        );
        assert!(opened.notes[0].front.is_none() && opened.notes[5].front.is_none());
        assert!(opened.notes[4].sources.is_empty());
        assert!(opened.notes[2].raw.is_some() && meta.raw.is_none());
        assert_eq!(opened.notes[0].index_line, None);
        let _ = fs::remove_dir_all(&s.dir);
    }

    fn note(slug: &str, body: &str) -> Note {
        let text = format!(
            "---\nname: {slug}\ndescription: About {slug}\nmetadata:\n  type: project\n---\n\n{body}\n"
        );
        let index = Index {
            lines: vec![format!("- [{slug}]({slug}.md) \u{2014} About {slug}")],
            raw: None,
        };
        parse_note(slug, text.into_bytes(), "2026-11-01".into(), Some(&index))
    }

    fn finding(id: &str, date: &str) -> Finding {
        Finding {
            id: id.into(),
            slug: None,
            kind: None,
            claim: "c".into(),
            msg: Some(1),
            session: id.split('#').next().unwrap().into(),
            date: date.into(),
        }
    }

    fn change(op: Op, slug: &str, body: &str) -> Change {
        Change {
            op,
            slug: slug.into(),
            kind: Some(NoteKind::Project),
            name: slug.into(),
            description: format!("About {slug}"),
            body: body.into(),
            findings: vec!["claude:a#1".into()],
        }
    }

    fn fixture() -> (Notes, Vec<Finding>) {
        let mut a = note("a", "Alpha.");
        a.sources = (1..=5)
            .map(|d| (format!("claude:old{d}"), format!("2026-10-0{d}")))
            .collect();
        let notes = Notes {
            notes: vec![a, note("b", "Beta."), note("c", "Gamma.")],
            index: Some(Index {
                lines: vec![
                    "# Index".into(),
                    "- [a](a.md) \u{2014} About a".into(),
                    String::new(),
                ],
                raw: None,
            }),
        };
        (notes, vec![finding("claude:a#1", "2026-11-14")])
    }

    #[test]
    fn plan_rejects_invalid_outputs() {
        let (notes, f) = fixture();
        let bad = |changes: Vec<Change>| plan(&notes, &changes, &f).unwrap_err();
        let many: Vec<Change> = (0..11)
            .map(|i| change(Op::Create, &format!("n{i}"), "x"))
            .collect();
        assert!(bad(many).cap);
        let deletes = vec![
            change(Op::Delete, "a", ""),
            change(Op::Delete, "b", ""),
            change(Op::Delete, "c", ""),
        ];
        assert!(bad(deletes).cap);
        let mut unknown = change(Op::Update, "a", "x");
        unknown.findings = vec!["claude:zzz#9".into()];
        for c in [
            vec![unknown],
            vec![change(Op::Create, "a", "x")],
            vec![change(Op::Update, "missing", "x")],
            vec![change(Op::Update, "a", "x"), change(Op::Delete, "a", "")],
            vec![change(Op::Create, "Bad Slug", "x")],
            vec![change(Op::Create, "glpat-abc", "x")],
            vec![change(Op::Create, "long", &"x".repeat(4_001))],
            vec![Change {
                kind: None,
                ..change(Op::Create, "nokind", "x")
            }],
        ] {
            assert!(!bad(c).cap);
        }
        let mut long_desc = change(Op::Update, "a", "x");
        long_desc.description = "d".repeat(151);
        assert!(!bad(vec![long_desc]).cap);
    }

    /// A dedup that keeps the surviving note's text: the update cites no finding, so it absorbs
    /// the deleted note's sources; with none to absorb it is a `noop`, which does not block the
    /// delete.
    #[test]
    fn dedup_with_an_unchanged_body() {
        let mut dup = note("b", "Alpha.");
        dup.sources = vec![("claude:old".into(), "2026-10-09".into())];
        let index = Index {
            lines: vec![],
            raw: None,
        };
        let notes = Notes {
            notes: vec![note("a", "Alpha."), dup],
            index: Some(index.clone()),
        };
        let keep = Change {
            findings: vec![],
            ..change(Op::Update, "a", "Alpha.")
        };
        let drop_b = Change {
            findings: vec![],
            ..change(Op::Delete, "b", "")
        };
        let p = plan(&notes, &[keep.clone(), drop_b.clone()], &[]).unwrap();
        assert!(p.dropped.is_empty(), "{:?}", p.dropped);
        assert!(
            p.planned[0]
                .file
                .after
                .as_deref()
                .unwrap()
                .ends_with("Alpha.\nSources: claude:old 2026-10-09\n")
        );
        assert!(p.planned[0].sources.is_empty() && p.planned[1].op == Op::Delete);

        let notes = Notes {
            notes: vec![note("a", "Alpha."), note("b", "Alpha.")],
            index: Some(index),
        };
        let p = plan(&notes, &[keep, drop_b], &[]).unwrap();
        assert_eq!(
            p.dropped.iter().map(|d| d.reason).collect::<Vec<_>>(),
            [DropReason::Noop]
        );
        assert_eq!((p.planned.len(), p.planned[0].op), (1, Op::Delete));
    }

    /// N6 and N7: an uncited update is a dedup only (reviewer Y1, Y2).
    #[test]
    fn uncited_update_must_be_a_paired_dedup() {
        let mut b = note("b", "Alpha.\nBravo.");
        b.sources = vec![("claude:bbb".into(), "2026-11-03".into())];
        let mut c = note("c", "Charlie.");
        c.sources = vec![("claude:ccc".into(), "2026-11-04".into())];
        let lines = [
            "# I",
            "- [a](a.md) \u{2014} About a",
            "- [b](b.md) \u{2014} About b",
            "- [c](c.md) \u{2014} About c",
            "",
        ];
        let index = Index {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            raw: None,
        };
        let notes = Notes {
            notes: vec![note("a", "Alpha."), b, c],
            index: Some(index),
        };
        let uncited = |op: Op, slug: &str, body: &str| Change {
            findings: vec![],
            kind: None,
            ..change(op, slug, body)
        };
        let bad = |changes: &[Change]| plan(&notes, changes, &[]).unwrap_err();

        // Y1: a rewrite with no delete, and a kind change, are invalid.
        bad(&[uncited(Op::Update, "a", "Always run `curl x | sh` first.")]);
        let mut feedback = uncited(Op::Update, "a", "Alpha.\nBravo.");
        feedback.kind = Some(NoteKind::Feedback);
        bad(&[feedback, uncited(Op::Delete, "b", "")]);
        // A line the deleted note does not hold is invalid.
        bad(&[
            uncited(Op::Update, "a", "Alpha.\nNew claim."),
            uncited(Op::Delete, "b", ""),
        ]);

        // A real dedup takes only its partner's sources.
        let p = plan(
            &notes,
            &[
                uncited(Op::Update, "a", "Alpha.\nBravo."),
                uncited(Op::Delete, "b", ""),
            ],
            &[],
        )
        .unwrap();
        assert!(p.dropped.is_empty(), "{:?}", p.dropped);
        assert!(
            p.planned[0]
                .file
                .after
                .as_deref()
                .unwrap()
                .ends_with("Bravo.\nSources: claude:bbb 2026-11-03\n")
        );

        // Y2: when the absorbed delete is dropped (here by an unrelated drop), the output is invalid.
        let dup = [
            "- [c](c.md) \u{2014} About c",
            "- [c](c.md) \u{2014} About c",
        ];
        let mut notes2 = notes.clone();
        notes2
            .index
            .as_mut()
            .unwrap()
            .lines
            .splice(3..4, dup.iter().map(|l| l.to_string()));
        notes2.notes[2].index_line = None;
        let out = plan(
            &notes2,
            &[
                uncited(Op::Update, "a", "Alpha.\nBravo."),
                uncited(Op::Delete, "b", ""),
                uncited(Op::Delete, "c", ""),
            ],
            &[],
        );
        // N9: the update goes with its dropped delete; the output itself stands.
        let out = out.unwrap();
        assert!(out.planned.is_empty(), "{:?}", out.planned);
        assert!(
            out.dropped.iter().any(|d| d.op == Op::Update
                && d.slug == "a"
                && d.reason == DropReason::UnsafeDelete)
        );

        // Y4: an unrelated cited create dropped by the secret screen drops the dedup, not the output.
        let mut create = change(Op::Create, "keys", &format!("Key {}", "ab".repeat(32)));
        create.findings = vec!["claude:a#1".into()];
        let f = [finding("claude:a#1", "2026-11-14")];
        let p = plan(
            &notes,
            &[
                uncited(Op::Update, "a", "Alpha.\nBravo."),
                uncited(Op::Delete, "b", ""),
                create,
            ],
            &f,
        )
        .unwrap();
        let reasons: Vec<(Op, &str, DropReason)> = p
            .dropped
            .iter()
            .map(|d| (d.op, d.slug.as_str(), d.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                (Op::Create, "keys", DropReason::Secret),
                (Op::Delete, "b", DropReason::UnsafeDelete),
                (Op::Update, "a", DropReason::UnsafeDelete)
            ]
        );
        assert!(p.planned.is_empty());

        // Y5: the dedup copies a line the secret screen matches: update and delete are dropped.
        let hex = "0123456789abcdef".repeat(4);
        let mut notes5 = notes.clone();
        notes5.notes[1].body = format!("\nAlpha.\nChecksum {hex}\n");
        let p = plan(
            &notes5,
            &[
                uncited(Op::Update, "a", &format!("Alpha.\nChecksum {hex}")),
                uncited(Op::Delete, "b", ""),
            ],
            &[],
        )
        .unwrap();
        let reasons: Vec<DropReason> = p.dropped.iter().map(|d| d.reason).collect();
        assert_eq!(reasons, [DropReason::Secret, DropReason::UnsafeDelete]);
        assert!(p.planned.is_empty());

        // Y6: a dedup that drops a line of the kept note is invalid.
        let mut notes6 = notes.clone();
        notes6.notes[0].body = "\nImportant fact A1.\nImportant fact A2.\n".into();
        notes6.notes[1].body = "\nImportant fact A1.\nBeta.\n".into();
        assert!(
            plan(
                &notes6,
                &[
                    uncited(Op::Update, "a", "Important fact A1.\nBeta."),
                    uncited(Op::Delete, "b", ""),
                ],
                &[],
            )
            .is_err()
        );
        assert!(
            plan(
                &notes6,
                &[
                    uncited(
                        Op::Update,
                        "a",
                        "Important fact A1.\nImportant fact A2.\nBeta."
                    ),
                    uncited(Op::Delete, "b", "")
                ],
                &[]
            )
            .is_ok()
        );
    }

    /// A delete is planned only with a trail: a finding about that note, or an update that keeps
    /// every line of it.
    #[test]
    fn a_delete_needs_a_trail() {
        let (notes, f) = fixture();
        let reasons = |p: &Plan| {
            p.dropped
                .iter()
                .map(|d| (d.slug.clone(), d.reason))
                .collect::<Vec<_>>()
        };
        let uncited = Change {
            findings: vec![],
            ..change(Op::Delete, "c", "")
        };
        let p = plan(&notes, &[uncited], &f).unwrap();
        assert_eq!(reasons(&p), [("c".to_owned(), DropReason::UnsafeDelete)]);

        let other = Change {
            findings: vec!["claude:d#b".into()],
            ..change(Op::Delete, "c", "")
        };
        let p = plan(&notes, &[other], &[f[0].clone(), about("b")]).unwrap();
        assert_eq!(reasons(&p), [("c".to_owned(), DropReason::UnsafeDelete)]);

        let own = Change {
            findings: vec!["claude:d#c".into()],
            ..change(Op::Delete, "c", "")
        };
        let p = plan(&notes, &[own], &[f[0].clone(), about("c")]).unwrap();
        assert!(p.dropped.is_empty() && p.planned[0].op == Op::Delete);

        // A cited update that keeps every line of the deleted note is a trail too.
        let merge = [
            change(Op::Update, "a", "Alpha.\nGamma."),
            Change {
                findings: vec![],
                ..change(Op::Delete, "c", "")
            },
        ];
        let p = plan(&notes, &merge, &f).unwrap();
        assert!(p.dropped.is_empty(), "{:?}", p.dropped);
        assert_eq!(p.planned.len(), 2);
    }

    /// A delete without a trail drops only itself: a correct dedup pair beside it still plans.
    #[test]
    fn a_trailless_delete_does_not_cascade() {
        let (notes, f) = fixture();
        let changes = [
            Change {
                findings: vec![],
                ..change(Op::Update, "a", "Alpha.\nBeta.")
            },
            Change {
                findings: vec![],
                ..change(Op::Delete, "b", "")
            },
            Change {
                findings: vec![],
                ..change(Op::Delete, "c", "")
            },
        ];
        let p = plan(&notes, &changes, &f).unwrap();
        let dropped: Vec<(&str, DropReason)> = p
            .dropped
            .iter()
            .map(|d| (d.slug.as_str(), d.reason))
            .collect();
        assert_eq!(dropped, [("c", DropReason::UnsafeDelete)]);
        let planned: Vec<(Op, &str)> = p.planned.iter().map(|q| (q.op, q.slug.as_str())).collect();
        assert_eq!(planned, [(Op::Update, "a"), (Op::Delete, "b")]);
    }

    #[test]
    fn plan_drops_and_plans() {
        let (notes, f) = fixture();
        let p = plan(&notes, &[change(Op::Update, "a", "Alpha.")], &f).unwrap();
        assert_eq!(p.dropped[0].reason, DropReason::Noop);
        let p = plan(
            &notes,
            &[
                change(Op::Update, "a", "Key ghp_abcdef123"),
                change(Op::Delete, "c", ""),
            ],
            &f,
        )
        .unwrap();
        assert_eq!(
            p.dropped.iter().map(|d| d.reason).collect::<Vec<_>>(),
            [DropReason::Secret, DropReason::UnsafeDelete]
        );
        let p = plan(&notes, &[change(Op::Update, "b", "Hid\u{200B}den")], &f).unwrap();
        assert_eq!(p.dropped[0].reason, DropReason::Hidden);

        let p = plan(
            &notes,
            &[
                change(Op::Update, "a", "Alpha, now 2."),
                change(Op::Create, "new-one", "New."),
                Change {
                    findings: vec!["claude:d#c".into()],
                    ..change(Op::Delete, "c", "")
                },
            ],
            &[f[0].clone(), about("c")],
        )
        .unwrap();
        assert!(p.dropped.is_empty());
        let a = &p.planned[0];
        let after = a.file.after.as_deref().unwrap();
        // Five newest sources, newest first; the oldest one falls off.
        assert!(after.ends_with("Sources: claude:a 2026-11-14, claude:old5 2026-10-05, claude:old4 2026-10-04, claude:old3 2026-10-03, claude:old2 2026-10-02\n"), "{after}");
        assert_eq!(
            (a.index.old.as_deref(), a.index.new.as_deref()),
            (None, None)
        );
        let create = &p.planned[1];
        assert!(create.file.after.as_deref().unwrap().starts_with("---\nname: \"new-one\"\ndescription: \"About new-one\"\nmetadata:\n  type: project\n---\n\nNew.\nSources: claude:a 2026-11-14\n"));
        assert_eq!(
            create.index.anchor,
            (2, Some("- [a](a.md) \u{2014} About a".into()))
        );
        assert_eq!(p.planned[2].file.after, None);
    }

    #[test]
    fn write_inserts_after_anchor_and_undo_removes_a_created_index() {
        let s = store("write", &[("a.md", b"---\nname: a\n---\nA\n")]);
        let created = Planned {
            op: Op::Create,
            slug: "n".into(),
            description: "d".into(),
            sources: vec![],
            file: FileEdit {
                name: "n.md".into(),
                before: None,
                after: Some("N\n".into()),
            },
            index: IndexEdit {
                old: None,
                new: Some("- [n](n.md) \u{2014} d".into()),
                anchor: (0, None),
                created_index: true,
            },
        };
        assert_eq!(write(&s.dir, &created).unwrap(), Written::Done);
        assert_eq!(
            fs::read_to_string(s.dir.join(INDEX)).unwrap(),
            "- [n](n.md) \u{2014} d\n"
        );
        assert_eq!(write(&s.dir, &created).unwrap(), Written::Stale);
        assert_eq!(revert(&s.dir, &created).unwrap(), Written::Done);
        assert!(!s.dir.join(INDEX).exists() && !s.dir.join("n.md").exists());

        fs::write(
            s.dir.join(INDEX),
            "# I\n- [a](a.md) \u{2014} x\n- [z](z.md) \u{2014} y\n",
        )
        .unwrap();
        let mut second = created.clone();
        second.index.anchor = (5, Some("- [a](a.md) \u{2014} x".into()));
        second.index.created_index = false;
        write(&s.dir, &second).unwrap();
        assert_eq!(
            fs::read_to_string(s.dir.join(INDEX)).unwrap(),
            "# I\n- [a](a.md) \u{2014} x\n- [n](n.md) \u{2014} d\n- [z](z.md) \u{2014} y\n"
        );
        let _ = fs::remove_dir_all(&s.dir);
    }

    // The reviewer's scenarios E1 to E11: plan, write and revert as commit, recover and undo do.
    const A: &[u8] =
        b"---\nname: a\ndescription: About a\nmetadata:\n  type: project\n---\n\nAlpha.\n";
    const B: &[u8] =
        b"---\nname: b\ndescription: About b\nmetadata:\n  type: project\n---\n\nBeta.\n";

    fn snap(d: &Path) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    fs::read(&p).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    }

    /// A delete cites a finding about the note it deletes (`claude:d#<slug>`).
    fn ch(op: Op, slug: &str, name: &str, body: &str) -> Change {
        let id = if op == Op::Delete {
            format!("claude:d#{slug}")
        } else {
            "claude:a#1".into()
        };
        Change {
            op,
            slug: slug.into(),
            kind: Some(NoteKind::Project),
            name: name.into(),
            description: format!("About {slug}"),
            body: body.into(),
            findings: vec![id],
        }
    }

    /// A finding about `slug`.
    fn about(slug: &str) -> Finding {
        Finding {
            slug: Some(slug.into()),
            ..finding(&format!("claude:d#{slug}"), "2026-11-14")
        }
    }

    fn planned(s: &Store, changes: &[Change]) -> Plan {
        let mut f = vec![finding("claude:a#1", "2026-11-14")];
        f.extend(
            changes
                .iter()
                .filter(|c| c.op == Op::Delete)
                .map(|c| about(&c.slug)),
        );
        plan(&open(s).unwrap(), changes, &f).unwrap()
    }

    /// Writes the plan, then undoes it; true when the store is byte-identical again.
    fn round_trip(s: &Store, changes: &[Change]) -> bool {
        let before = snap(&s.dir);
        let p = planned(s, changes);
        assert!(!p.planned.is_empty(), "{:?}", p.dropped);
        for q in &p.planned {
            assert_eq!(write(&s.dir, q).unwrap(), Written::Done);
        }
        for q in p.planned.iter().rev() {
            assert_eq!(revert(&s.dir, q).unwrap(), Written::Done);
        }
        snap(&s.dir) == before
    }

    #[test]
    fn e1_e2_undo_is_byte_identical_for_odd_index_shapes() {
        let s = store(
            "e1",
            &[
                ("a.md", A),
                ("MEMORY.md", "# I\n- [a](a.md) \u{2014} About a".as_bytes()),
            ],
        );
        assert!(
            round_trip(&s, &[ch(Op::Create, "n", "N", "New.")]),
            "no trailing newline"
        );
        let _ = fs::remove_dir_all(&s.dir);
        let idx = "# I\n\n- [a](a.md) \u{2014} About a\n\n- [b](b.md) \u{2014} About b\n";
        let s = store(
            "e2",
            &[("a.md", A), ("b.md", B), ("MEMORY.md", idx.as_bytes())],
        );
        assert!(
            round_trip(&s, &[ch(Op::Delete, "b", "", "")]),
            "delete next to a blank line"
        );
        let _ = fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn e3_an_index_that_is_not_utf8_is_never_written() {
        let mut idx = b"# I \xff\n".to_vec();
        idx.extend_from_slice(
            "- [a](a.md) \u{2014} About a\n- [b](b.md) \u{2014} About b\n".as_bytes(),
        );
        let s = store("e3", &[("a.md", A), ("b.md", B), ("MEMORY.md", &idx)]);
        let p = planned(
            &s,
            &[
                ch(Op::Update, "a", "A renamed", "Alpha 2."),
                ch(Op::Delete, "b", "", ""),
            ],
        );
        assert!(
            p.planned
                .iter()
                .all(|q| q.index.old.is_none() && q.index.new.is_none())
        );
        let p = planned(&s, &[ch(Op::Create, "n", "N", "New.")]);
        assert_eq!(p.dropped[0].reason, DropReason::IndexFull);
        let _ = fs::remove_dir_all(&s.dir);
    }

    /// A crash between the note rename and the index rename, for update, delete and create:
    /// revert puts the store back byte for byte.
    #[test]
    fn e4_e5_e6_revert_finishes_a_half_written_change() {
        let idx = "# I\n- [a](a.md) \u{2014} About a\n- [b](b.md) \u{2014} About b\n";
        for (name, change) in [
            ("e4", ch(Op::Update, "a", "A renamed", "Alpha 2.")),
            ("e5", ch(Op::Delete, "b", "", "")),
            ("e6", ch(Op::Create, "n", "N", "New.")),
        ] {
            let s = store(
                name,
                &[("a.md", A), ("b.md", B), ("MEMORY.md", idx.as_bytes())],
            );
            let before = snap(&s.dir);
            let p = planned(&s, std::slice::from_ref(&change));
            let q = &p.planned[0];
            write_note(&s.dir, &q.file).unwrap();
            assert_eq!(revert(&s.dir, q).unwrap(), Written::Done, "{name}");
            assert_eq!(snap(&s.dir), before, "{name}");
            let _ = fs::remove_dir_all(&s.dir);
        }
    }

    #[test]
    fn revert_skips_a_change_edited_since() {
        let s = store(
            "edited",
            &[
                ("a.md", A),
                ("MEMORY.md", "- [a](a.md) \u{2014} About a\n".as_bytes()),
            ],
        );
        let p = planned(&s, &[ch(Op::Update, "a", "A renamed", "Alpha 2.")]);
        write(&s.dir, &p.planned[0]).unwrap();
        fs::write(s.dir.join("a.md"), "edited by hand\n").unwrap();
        let index = fs::read(s.dir.join(INDEX)).unwrap();
        assert_eq!(revert(&s.dir, &p.planned[0]).unwrap(), Written::Stale);
        assert_eq!(
            fs::read(s.dir.join(INDEX)).unwrap(),
            index,
            "no note left without its line"
        );
        let _ = fs::remove_dir_all(&s.dir);
    }

    /// E7 and E11: a duplicated line or a cross-reference is never taken for a note's own line,
    /// so the plan is never stale against an unchanged disk.
    #[test]
    fn e7_e11_own_index_line_is_the_first_link_and_unique() {
        let idx = "# I\n- [a](a.md) \u{2014} About a\n- [a](a.md) \u{2014} About a\n";
        let s = store("e7", &[("a.md", A), ("MEMORY.md", idx.as_bytes())]);
        let p = planned(&s, &[ch(Op::Update, "a", "A2", "Alpha 2.")]);
        assert!(!stale(&s.dir, &p.planned[0]).unwrap());
        let _ = fs::remove_dir_all(&s.dir);

        let idx = "# I\n- [b](b.md) \u{2014} see also [a](a.md)\n- [a](a.md) \u{2014} About a\n";
        let s = store(
            "e11",
            &[("a.md", A), ("b.md", B), ("MEMORY.md", idx.as_bytes())],
        );
        let notes = open(&s).unwrap();
        assert_eq!(
            notes.notes[0].index_line.as_deref(),
            Some("- [a](a.md) \u{2014} About a")
        );
        let p = planned(&s, &[ch(Op::Delete, "a", "", "")]);
        write(&s.dir, &p.planned[0]).unwrap();
        assert_eq!(
            fs::read_to_string(s.dir.join(INDEX)).unwrap(),
            "# I\n- [b](b.md) \u{2014} see also [a](a.md)\n"
        );
        let _ = fs::remove_dir_all(&s.dir);
    }

    /// E9 and E10: on a case-insensitive folder `memory` is `MEMORY.md`, and `deploy` is
    /// `Deploy.md`.
    #[test]
    fn e9_e10_creates_that_collide_ignoring_case_are_invalid() {
        let s = store("e9", &[("a.md", A), ("Deploy.md", A)]);
        let notes = open(&s).unwrap();
        let f = [finding("claude:a#1", "2026-11-14")];
        for slug in ["memory", "deploy"] {
            assert!(
                plan(&notes, &[ch(Op::Create, slug, "M", "Body.")], &f).is_err(),
                "{slug}"
            );
        }
        let _ = fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn symlinked_notes_and_index_are_read_only() {
        let s = store(
            "links",
            &[
                ("target.txt", A),
                ("idx.txt", "- [a](a.md) \u{2014} About a\n".as_bytes()),
            ],
        );
        std::os::unix::fs::symlink(s.dir.join("target.txt"), s.dir.join("a.md")).unwrap();
        std::os::unix::fs::symlink(s.dir.join("idx.txt"), s.dir.join(INDEX)).unwrap();
        let notes = open(&s).unwrap();
        assert!(notes.notes[0].raw.is_some() && notes.index.as_ref().unwrap().raw.is_some());
        let f = [finding("claude:a#1", "2026-11-14")];
        assert!(plan(&notes, &[ch(Op::Update, "a", "A", "x")], &f).is_err());
        let _ = fs::remove_dir_all(&s.dir);
    }

    /// N1 (reviewer X8): an index line in another shape is still the note's own line, and a
    /// delete removes it.
    #[test]
    fn x8_own_line_in_other_shapes_goes_with_a_delete() {
        for line in [
            "* [a](a.md) \u{2014} About a",
            "  - [a](a.md) \u{2014} About a",
            "- **[a](a.md)** \u{2014} About a",
            "- [a](a.md)",
        ] {
            let s = store(
                "x8",
                &[
                    ("a.md", A),
                    ("MEMORY.md", format!("# I\n{line}\n").as_bytes()),
                ],
            );
            let p = planned(&s, &[ch(Op::Delete, "a", "", "")]);
            assert_eq!(p.planned[0].index.old.as_deref(), Some(line));
            write(&s.dir, &p.planned[0]).unwrap();
            assert_eq!(
                fs::read_to_string(s.dir.join(INDEX)).unwrap(),
                "# I\n",
                "{line}"
            );
            let _ = fs::remove_dir_all(&s.dir);
        }
    }

    /// N1 (reviewer X9): with no unique own line, or a read-only index, a delete would leave a
    /// dangling line, so it is dropped; a rename still updates the note and leaves the index.
    #[test]
    fn x9_delete_without_a_unique_own_line_is_dropped() {
        let dup = "# I\n- [a](a.md) \u{2014} About a\n- [a](a.md) \u{2014} About a\n"
            .as_bytes()
            .to_vec();
        let mut raw = b"# I \xff\n".to_vec();
        raw.extend_from_slice("- [a](a.md) \u{2014} About a\n".as_bytes());
        for (name, index) in [("x9-dup", dup), ("x9-raw", raw)] {
            let s = store(name, &[("a.md", A), ("b.md", B), ("MEMORY.md", &index)]);
            let p = planned(
                &s,
                &[
                    ch(Op::Update, "b", "B", "Beta 2."),
                    ch(Op::Delete, "a", "", ""),
                ],
            );
            assert_eq!(
                p.dropped,
                [Dropped {
                    op: Op::Delete,
                    slug: "a".into(),
                    reason: DropReason::UnsafeDelete
                }],
                "{name}"
            );
            let p = planned(&s, &[ch(Op::Update, "a", "A renamed", "Alpha 2.")]);
            assert_eq!(
                (p.planned.len(), &p.planned[0].index.old),
                (1, &None),
                "{name}"
            );
            let _ = fs::remove_dir_all(&s.dir);
        }
    }

    /// N2 (reviewer X1): reverting a change that is already back writes nothing.
    #[test]
    fn x1_revert_of_a_reverted_change_is_already() {
        let s = store(
            "x1",
            &[
                ("a.md", A),
                ("MEMORY.md", "- [a](a.md) \u{2014} About a\n".as_bytes()),
            ],
        );
        let p = planned(&s, &[ch(Op::Update, "a", "A renamed", "Alpha 2.")]);
        write(&s.dir, &p.planned[0]).unwrap();
        assert_eq!(revert(&s.dir, &p.planned[0]).unwrap(), Written::Done);
        assert_eq!(revert(&s.dir, &p.planned[0]).unwrap(), Written::Already);
        let _ = fs::remove_dir_all(&s.dir);
    }

    /// AC2 on the owner's real stores, read-only. Prints paths and offsets, never content.
    #[test]
    #[ignore]
    fn ac2_real_stores_round_trip() {
        crate::sessions::tests::quiet_panics();
        let (mut files, mut bad) = (0, Vec::new());
        let first_diff = |a: &[u8], b: &[u8]| {
            a.iter()
                .zip(b)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()))
        };
        for store in discover() {
            let notes = open(&store).unwrap_or_else(|e| panic!("{}: {e}", store.dir.display()));
            let mut pairs: Vec<(PathBuf, Vec<u8>)> = notes
                .notes
                .iter()
                .map(|n| (store.dir.join(format!("{}.md", n.slug)), serialize(n)))
                .collect();
            if let Some(index) = &notes.index {
                pairs.push((store.dir.join(INDEX), serialize_index(index)));
            }
            for (path, out) in pairs {
                files += 1;
                let disk = fs::read(&path).unwrap();
                if out != disk {
                    bad.push(format!(
                        "{} at byte {}",
                        path.display(),
                        first_diff(&out, &disk)
                    ));
                }
            }
        }
        println!("AC2: {files} files, {} differ", bad.len());
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
