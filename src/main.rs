//! `dreamcatcher`: CLI, power check and orchestration of a tick.

use ai_dreamcatcher::dream::{self, Backend};
use ai_dreamcatcher::exec;
use ai_dreamcatcher::journal::{self, Commit, Journal};
use ai_dreamcatcher::memory::{self, Notes};
use ai_dreamcatcher::model::{AbortReason, Config, Finding, Plan, Seen, Session, Source, Time};
use ai_dreamcatcher::screen;
use ai_dreamcatcher::sessions::{self, Error as SessionError, Listing, Map};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

/// Notes a model call may carry, in characters.
const NOTES_BUDGET: usize = 200_000;
const LOG_LIMIT: u64 = 1 << 20;
const PMSET_TIMEOUT: Duration = Duration::from_secs(10);
const POND_TIMEOUT: Duration = Duration::from_secs(120);
const USAGE: &str = "usage: dreamcatcher init [--source pond|files] | run [--dry-run] | status | log [<run>] | undo <run> | retry <ref> | smoke";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = match args.as_slice() {
        ["init"] => init(None),
        ["init", "--source", s @ ("pond" | "files")] => init(Some(s)),
        ["run"] => run(false),
        ["run", "--dry-run"] => run(true),
        ["status"] => status(),
        ["log"] => log(None),
        ["log", run] => log(Some(run)),
        ["undo", run] => undo(run),
        ["retry", reference] => retry(reference),
        ["smoke"] => smoke(),
        _ => Err(Fail::Usage(USAGE.into())),
    };
    match code {
        Ok(code) => ExitCode::from(code),
        Err(Fail::Usage(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
        Err(Fail::Error(msg)) => {
            eprintln!("dreamcatcher: {}", screen::redact(&msg));
            ExitCode::from(1)
        }
    }
}

enum Fail {
    /// Exit 2: usage, or not initialised.
    Usage(String),
    /// Exit 1.
    Error(String),
}

impl From<exec::Error> for Fail {
    fn from(e: exec::Error) -> Self {
        Fail::Error(e.0)
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Error(e.to_string())
    }
}

type Out = Result<u8, Fail>;

/// Every clock read uses `DREAMCATCHER_NOW` when it is set.
fn now() -> Time {
    std::env::var("DREAMCATCHER_NOW")
        .ok()
        .and_then(|s| Time::parse(&s))
        .unwrap_or_else(|| Time::from_system(std::time::SystemTime::now()))
}

fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

fn config_path() -> PathBuf {
    home().join(".config/ai-dreamcatcher/config.json")
}

fn load_config() -> Result<Config, Fail> {
    let text = fs::read_to_string(config_path()).map_err(|_| {
        Fail::Usage("dreamcatcher: not initialised, run `dreamcatcher init`".into())
    })?;
    serde_json::from_str(&text).map_err(|e| Fail::Usage(format!("dreamcatcher: config.json: {e}")))
}

fn source_of(config: &Config) -> Result<Source, Fail> {
    match (config.source.as_str(), &config.pond) {
        ("files", _) => Ok(Source::Files),
        ("pond", Some(pond)) => Ok(Source::Pond(pond.clone())),
        _ => Err(Fail::Usage(
            "dreamcatcher: config.json: source must be files, or pond with a pond path".into(),
        )),
    }
}

fn backend(config: &Config) -> Backend {
    Backend {
        claude: config.claude.clone(),
        dir: journal::state_dir().join("call"),
    }
}

/// An absolute path to `name` on `PATH` or in `~/.local/bin`.
fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([home().join(".local/bin")])
        .map(|d| d.join(name))
        .find(|p| p.is_absolute() && p.is_file())
}

fn init(source: Option<&str>) -> Out {
    let claude = find("claude")
        .ok_or_else(|| Fail::Error("cannot find `claude` on PATH or in ~/.local/bin".into()))?;
    let pond = find("pond");
    let pond_up = || {
        pond.as_ref().is_some_and(|bin| {
            exec::run(Command::new(bin).arg("status"), None, POND_TIMEOUT).is_ok()
        })
    };
    let source = match source {
        Some("pond") if pond.is_none() => {
            return Err(Fail::Usage(
                "dreamcatcher: --source pond, but pond is not installed".into(),
            ));
        }
        Some(s) => s.to_owned(),
        None if pond_up() => "pond".into(),
        None => "files".into(),
    };
    // A second init keeps the first init time and the user's scope lists.
    let old: Option<Config> = fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let config = Config {
        init: old
            .as_ref()
            .map_or_else(|| now().rfc3339(), |c| c.init.clone()),
        claude,
        pond,
        source,
        exclude: old.as_ref().map_or_else(Vec::new, |c| c.exclude.clone()),
        cross_provider: old.map_or_else(Vec::new, |c| c.cross_provider),
    };
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    fs::write(
        &path,
        serde_json::to_string_pretty(&config).map_err(exec::Error::from)? + "\n",
    )?;
    let state = journal::ensure_state()?;
    println!("wrote {} (source: {})", path.display(), config.source);
    dream::smoke(&backend(&config))?;
    println!("smoke: ok");
    let src = source_of(&config)?;
    match sessions::scopes(&src, memory::discover(), &config, now()) {
        Ok(map) => {
            for s in map.scopes.iter().filter(|s| s.hidden.is_none()) {
                println!(
                    "scope {}{}",
                    s.store.id,
                    s.skip.map_or(String::new(), |r| format!(" (skipped: {r})"))
                );
            }
        }
        Err(e) => println!("scopes: source down: {}", describe(&e)),
    }
    let exe = std::env::current_exe()?;
    println!(
        "\nlaunchd entry (installs nothing; save it as ~/Library/LaunchAgents/ai-dreamcatcher.plist and load it with\n\
         `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai-dreamcatcher.plist`):\n\n{}",
        plist(&exe, &state)
    );
    Ok(0)
}

fn plist(exe: &Path, state: &Path) -> String {
    let log = state.join("launchd.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>ai-dreamcatcher</string>
  <key>ProgramArguments</key>
  <array><string>/usr/bin/caffeinate</string><string>-i</string><string>{}</string><string>run</string></array>
  <key>StartCalendarInterval</key><dict><key>Minute</key><integer>0</integer></dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>"#,
        exe.display(),
        log.display(),
        log.display()
    )
}

fn smoke() -> Out {
    let config = load_config()?;
    journal::ensure_state()?;
    dream::smoke(&backend(&config))?;
    println!("smoke: ok");
    Ok(0)
}

/// True on battery: `pmset -g batt` lacks `AC Power`. An unreadable answer counts as AC.
fn on_battery() -> bool {
    exec::run(
        Command::new("pmset").args(["-g", "batt"]),
        None,
        PMSET_TIMEOUT,
    )
    .is_ok_and(|out| !out.contains("AC Power"))
}

fn describe(e: &SessionError) -> &str {
    match e {
        SessionError::SourceDown(m) | SessionError::Parse(m) => m,
    }
}

/// Empties `launchd.log` above 1 MB.
fn trim_log() {
    let log = journal::state_dir().join("launchd.log");
    if fs::metadata(&log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = fs::write(&log, "");
    }
}

fn instructions() -> String {
    ["CLAUDE.md", "AGENTS.md"]
        .iter()
        .filter_map(|f| {
            fs::read_to_string(home().join(".claude").join(f))
                .ok()
                .map(|t| format!("## ~/.claude/{f}\n\n{t}"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One tick.
fn run(dry: bool) -> Out {
    let config = load_config()?;
    let source = source_of(&config)?;
    let now = now();
    trim_log();
    if on_battery() {
        println!("on battery");
        return Ok(0);
    }
    let Some(journal) = journal::lock(now)? else {
        println!("busy");
        return Ok(0);
    };
    if !dry {
        journal.recover()?;
        journal.cleanup()?;
    }
    let run = now.run_id();
    let seen = journal::read();
    let source_down = |journal: &Journal, scope: &str, e: &str| -> Out {
        eprintln!("source down: {e}");
        if !dry {
            journal.abort(&run, scope, AbortReason::Source)?;
        }
        Ok(1)
    };
    let mut map = match sessions::scopes(&source, memory::discover(), &config, now) {
        Ok(m) => m,
        Err(e) => return source_down(&journal, "*", describe(&e)),
    };
    mark_oversized(&mut map);
    let listing = match sessions::list(&source, now, &mut map, &seen, &config) {
        Ok(l) => l,
        Err(e) => return source_down(&journal, "*", describe(&e)),
    };
    let due: Vec<(String, Vec<Session>)> =
        sessions::select(&listing, &map.scopes, &seen, &config, now, &source)
            .into_iter()
            .map(|(s, v)| (s.store.id.clone(), v))
            .collect();
    if due.is_empty() {
        println!("nothing due");
        return Ok(0);
    }
    let tick = Tick {
        journal: &journal,
        run: &run,
        dry,
        source: &source,
        config: &config,
        seen: &seen,
    };
    let mut code = 0;
    for (scope, pending) in due {
        match tick.scope(&mut map, &scope, &pending)? {
            Step::Done => {}
            Step::Failed => code = 1,
            Step::EndTick => return Ok(1),
        }
    }
    Ok(code)
}

struct Tick<'a> {
    journal: &'a Journal,
    run: &'a str,
    dry: bool,
    source: &'a Source,
    config: &'a Config,
    seen: &'a Seen,
}

enum Step {
    Done,
    Failed,
    EndTick,
}

impl Tick<'_> {
    fn abort(&self, scope: &str, reason: AbortReason) -> Result<(), Fail> {
        println!("{scope}: aborted ({})", journal::reason(reason));
        if !self.dry {
            self.journal.abort(self.run, scope, reason)?;
        }
        Ok(())
    }

    /// One scope run: extract each session, re-verify, merge, plan, commit.
    fn scope(&self, map: &mut Map, scope: &str, pending: &[Session]) -> Result<Step, Fail> {
        let Some(store) = map.get(scope).map(|s| s.store.clone()) else {
            return Ok(Step::Done);
        };
        let notes: Notes = memory::open(&store)?;
        let b = backend(self.config);
        let (mut findings, mut extracted) = (Vec::new(), Vec::new());
        for s in pending {
            let from = self.seen.consumed.get(&s.reference).map_or(0, |c| c.0);
            if !self.dry {
                self.journal
                    .attempt(self.run, scope, &s.reference, s.records)?;
            }
            let transcript = match sessions::render(self.source, s, from) {
                Ok(t) => t,
                Err(SessionError::SourceDown(e)) => {
                    eprintln!("source down: {e}");
                    self.abort(scope, AbortReason::Source)?;
                    return Ok(Step::EndTick);
                }
                Err(SessionError::Parse(e)) => {
                    eprintln!("{}: {}", s.reference, screen::redact(&e));
                    continue;
                }
            };
            match dream::extract(&b, &notes.notes, &transcript) {
                Ok(f) => {
                    if !self.dry {
                        self.journal.extracted(self.run, scope, &s.reference)?;
                    }
                    findings.extend(f);
                    extracted.push(s.clone());
                }
                Err(e) => eprintln!("{}: extract failed: {}", s.reference, dream::failure(&e)),
            }
        }
        if extracted.is_empty() {
            self.abort(scope, AbortReason::Backend)?;
            return Ok(Step::EndTick);
        }
        let (hits, checked) = match sessions::evidence(
            self.source,
            self.config,
            map,
            scope,
            &notes.notes,
            self.seen,
            pending,
        ) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("source down: {}", describe(&e));
                self.abort(scope, AbortReason::Source)?;
                return Ok(Step::EndTick);
            }
        };
        let mut hit_counts: HashMap<&str, usize> = HashMap::new();
        for h in &hits {
            *hit_counts
                .entry(h.slug.as_deref().unwrap_or(""))
                .or_default() += 1;
        }
        let checked: Vec<(String, usize)> = checked
            .iter()
            .map(|s| (s.clone(), hit_counts.get(s.as_str()).copied().unwrap_or(0)))
            .collect();
        let all: Vec<Finding> = findings.into_iter().chain(hits).collect();
        // One merge per scope run with a successful extract, findings or not, so duplicate notes
        // are consolidated even when the sessions add nothing new.
        let plan = match dream::merge(&b, &notes.notes, &all, &instructions(), |changes| {
            memory::plan(&notes, changes, &all).map_err(|i| (i.cap, i.reason))
        }) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{scope}: merge failed: {}", screen::redact(&e.reason));
                self.abort(
                    scope,
                    if e.cap {
                        AbortReason::Cap
                    } else {
                        AbortReason::Merge
                    },
                )?;
                return Ok(Step::Failed);
            }
        };
        if self.dry {
            print_plan(scope, &plan);
            return Ok(Step::Done);
        }
        let consumed: Vec<(String, u64)> = extracted
            .iter()
            .map(|s| (s.reference.clone(), s.records))
            .collect();
        match self
            .journal
            .commit(self.run, scope, &store.dir, &consumed, &plan, &checked)?
        {
            Commit::Done => {
                println!(
                    "{scope}: {} applied, {} dropped",
                    plan.planned.len(),
                    plan.dropped.len()
                );
                Ok(Step::Done)
            }
            Commit::Aborted => {
                println!("{scope}: aborted (stale)");
                Ok(Step::Failed)
            }
        }
    }
}

/// A store whose notes exceed the call budget is skipped, so it takes no scope slot (F15).
fn mark_oversized(map: &mut Map) {
    for s in map
        .scopes
        .iter_mut()
        .filter(|s| s.hidden.is_none() && s.skip.is_none())
    {
        if memory::open(&s.store).is_ok_and(|n| memory::size(&n) > NOTES_BUDGET) {
            s.skip = Some("notes exceed the call budget");
        }
    }
}

fn print_plan(scope: &str, plan: &Plan) {
    println!("{scope}: dry run, nothing written");
    for p in &plan.planned {
        println!("  {:?} {}: {}", p.op, p.slug, p.description);
    }
    for d in &plan.dropped {
        println!("  dropped {:?} {}: {:?}", d.op, d.slug, d.reason);
    }
}

fn status() -> Out {
    let config = load_config()?;
    let source = source_of(&config)?;
    let now = now();
    let seen = journal::read();
    let last_failure = seen
        .events
        .iter()
        .rev()
        .find(|e| e.ev == "aborted" && e.reason.as_deref() == Some("source"));
    println!(
        "source: {}{}{}",
        config.source,
        if matches!(source, Source::Files) {
            " (re-verify off)"
        } else {
            ""
        },
        last_failure.map_or(String::new(), |e| format!(
            ", last failure {} in run {}",
            e.t,
            e.run.as_deref().unwrap_or("?")
        ))
    );
    let mut map = match sessions::scopes(&source, memory::discover(), &config, now) {
        Ok(m) => m,
        Err(e) => {
            println!("source down: {}", describe(&e));
            return Ok(1);
        }
    };
    mark_oversized(&mut map);
    let listing = match sessions::list(&source, now, &mut map, &seen, &config) {
        Ok(l) => l,
        Err(e) => {
            println!("source down: {}", describe(&e));
            return Ok(1);
        }
    };
    if matches!(source, Source::Pond(_)) {
        println!(
            "codex rows in pond without an ordinal (older format, never read): {}",
            listing.codex_without_ordinal
        );
    }
    println!("\nscopes:");
    let mut secrets = Vec::new();
    for s in map.scopes.iter().filter(|s| s.hidden.is_none()) {
        let notes = memory::open(&s.store).ok();
        let count = notes.as_ref().map_or(0, |n| n.notes.len());
        if count == 0 && !listing.mapped.contains(&s.store.id) {
            continue;
        }
        let skip = s.skip;
        let done = seen
            .events
            .iter()
            .rev()
            .find(|e| e.ev == "done" && e.scope.as_deref() == Some(&s.store.id))
            .map_or("never".into(), |e| e.t.clone());
        println!(
            "  {}  notes {count}  last done {done}{}",
            s.store.id,
            skip.map_or(String::new(), |r| format!("  skipped: {r}"))
        );
        for n in notes.iter().flat_map(|n| &n.notes) {
            let text = format!("{}\n{}\n{}", n.name, n.description, n.body);
            if let Some(kind) = screen::secret(&text) {
                secrets.push(format!("  {}/{}  {kind}", s.store.id, n.slug));
            }
        }
    }
    print_sessions(&listing, &seen, &config, now, &source);
    println!("\ndropped changes:");
    for e in committed(&seen, "dropped") {
        println!(
            "  run {} {}  {:?} {}  {}",
            e.run.as_deref().unwrap_or(""),
            e.scope.as_deref().unwrap_or(""),
            e.op,
            e.slug.as_deref().unwrap_or(""),
            e.reason.as_deref().unwrap_or("")
        );
    }
    println!("\nnotes matching the secret screen:");
    for line in secrets {
        println!("{line}");
    }
    Ok(0)
}

fn print_sessions(listing: &Listing, seen: &Seen, config: &Config, now: Time, source: &Source) {
    let (mut pending, mut given_up) = (Vec::new(), Vec::new());
    let mut other: Vec<(&str, &str, &str)> = listing
        .skipped
        .iter()
        .map(|(r, scope, why)| (r.as_str(), scope.as_str(), *why))
        .collect();
    for s in &listing.sessions {
        match sessions::pending(s, seen, config, now, source) {
            None => pending.push(s),
            Some("given up") => given_up.push(s),
            Some(why) => other.push((&s.reference, s.scope.as_deref().unwrap_or(""), why)),
        }
    }
    println!("\npending sessions: {}", pending.len());
    for s in pending {
        println!(
            "  {}  scope {}  records {}",
            s.reference,
            s.scope.as_deref().unwrap_or(""),
            s.records
        );
    }
    println!("other window sessions: {}", other.len());
    for (r, scope, why) in other {
        println!("  {r}  scope {scope}  {why}");
    }
    println!("unmapped sessions: {}", listing.unmapped.len());
    for r in &listing.unmapped {
        println!("  {r}");
    }
    println!(
        "given-up sessions: {} (clear with `dreamcatcher retry <ref>`)",
        given_up.len()
    );
    for s in given_up {
        println!("  {}", s.reference);
    }
}

/// Events of kind `ev` whose run and scope have `done`.
fn committed<'a>(
    seen: &'a Seen,
    ev: &'a str,
) -> impl Iterator<Item = &'a ai_dreamcatcher::model::Event> {
    let done: std::collections::HashSet<(Option<&String>, Option<&String>)> = seen
        .events
        .iter()
        .filter(|e| e.ev == "done")
        .map(|e| (e.run.as_ref(), e.scope.as_ref()))
        .collect();
    seen.events
        .iter()
        .filter(move |e| e.ev == ev && done.contains(&(e.run.as_ref(), e.scope.as_ref())))
}

fn log(only: Option<&str>) -> Out {
    load_config()?;
    let seen = journal::read();
    let mut runs: Vec<(String, String)> = Vec::new();
    for e in seen
        .events
        .iter()
        .filter(|e| matches!(e.ev.as_str(), "done" | "undone") || crash(e))
    {
        let key = (
            e.run.clone().unwrap_or_default(),
            e.scope.clone().unwrap_or_default(),
        );
        if only.is_none_or(|r| r == key.0) && !runs.contains(&key) {
            runs.push(key);
        }
    }
    for (run, scope) in &runs {
        let mine = |e: &&ai_dreamcatcher::model::Event| {
            e.run.as_ref() == Some(run) && e.scope.as_ref() == Some(scope)
        };
        println!("run {run}  scope {scope}");
        // A crashed run's `applied` and `dropped` lines may be partial, and recover reverted it.
        let crashed = !seen.events.iter().filter(mine).any(|e| e.ev == "done");
        for e in seen.events.iter().filter(mine) {
            match e.ev.as_str() {
                "applied" | "dropped" if crashed => {}
                "applied" => println!(
                    "  {:?} {}: {}  sources: {}",
                    e.op.unwrap_or(ai_dreamcatcher::model::Op::Update),
                    e.slug.as_deref().unwrap_or(""),
                    e.description.as_deref().unwrap_or(""),
                    e.sources.as_deref().unwrap_or_default().join(", ")
                ),
                "dropped" => println!(
                    "  dropped {:?} {}: {}",
                    e.op.unwrap_or(ai_dreamcatcher::model::Op::Update),
                    e.slug.as_deref().unwrap_or(""),
                    e.reason.as_deref().unwrap_or("")
                ),
                "aborted" if crash(e) => {
                    println!("  crashed during its write; recover kept what it could not revert");
                }
                "undone" => println!(
                    "  undo of run {}: {} restored, skipped: {}",
                    e.of.as_deref().unwrap_or(""),
                    e.restored.unwrap_or(0),
                    e.skipped.as_deref().unwrap_or_default().join(", ")
                ),
                _ => {}
            }
        }
        // A run that committed, or whose crash snapshot recover kept, can be undone; an undo
        // run's own snapshot is for recover.
        let committed = seen
            .events
            .iter()
            .any(|e| (e.ev == "done" || crash(e)) && e.run.as_ref() == Some(run));
        if committed && journal::undoable(run) {
            println!("  dreamcatcher undo {run}");
        }
    }
    Ok(0)
}

fn crash(e: &ai_dreamcatcher::model::Event) -> bool {
    e.ev == "aborted" && e.reason.as_deref() == Some("crash")
}

fn undo(of: &str) -> Out {
    load_config()?;
    if Time::parse_run(of).is_none() {
        return Err(Fail::Usage(format!("dreamcatcher: not a run id: {of}")));
    }
    let now = now();
    let Some(journal) = journal::lock(now)? else {
        println!("busy");
        return Ok(0);
    };
    journal.recover()?;
    for (scope, restored, skipped) in journal.undo(&now.run_id(), of)? {
        println!(
            "{scope}: {restored} restored{}",
            if skipped.is_empty() {
                String::new()
            } else {
                format!(", skipped (edited since): {}", skipped.join(", "))
            }
        );
    }
    Ok(0)
}

fn retry(reference: &str) -> Out {
    load_config()?;
    let Some(journal) = journal::lock(now())? else {
        println!("busy");
        return Ok(0);
    };
    journal.recover()?;
    journal.retry(reference)?;
    println!("{reference}: strikes cleared");
    Ok(0)
}
