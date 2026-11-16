//! Acceptance criteria with stub `claude`, `pond` and `pmset`, each in a temp home.

mod common;

use common::{CASES, Home, NOW, SCOPE, expected, notes, session, uuid};
use std::fs;

const RUN: &str = "20261114T120000Z";

fn home(case: Option<&str>) -> Home {
    let h = Home::new(case);
    h.stub("claude");
    h.stub("pmset");
    h
}

/// The AC3 harness with stub answers in place of the model: proves the harness and the
/// pipeline, not the prompts.
#[test]
fn ac3_harness_runs_cases_with_a_stub_model() {
    for (case, env) in [
        (
            "new-fact",
            vec![("STUB_BODY", "Tests run with `just test`.")],
        ),
        ("nothing-durable", vec![("STUB_EXTRACT", "none")]),
        ("sdk-driven", vec![]),
        (
            "codex-opted-in",
            vec![("STUB_BODY", "Lint with `ruff check --fix`.")],
        ),
    ] {
        let mut h = home(Some(case));
        for (k, v) in env {
            h.set(k, v);
        }
        h.init_files();
        h.ok(&["run"], 0);
        let fails = h.check(&expected(case), RUN);
        assert!(fails.is_empty(), "{case}: {fails:?}");
    }
}

/// G5: the secret never reaches a model call, the store or the state folder.
#[test]
fn ac3_secret_case_redacts_before_any_call() {
    let mut h = home(Some("secret"));
    let inputs = h.dir.parent().unwrap().join("inputs");
    h.set("STUB_INPUTS", inputs.to_string_lossy());
    h.init_files();
    h.ok(&["run"], 0);
    assert!(h.check(&expected("secret"), RUN).is_empty());
    let sent: String = common::walk(&inputs)
        .iter()
        .map(|p| fs::read_to_string(p).unwrap())
        .collect();
    assert!(sent.contains("[redacted]"));
    assert!(!sent.contains("FIXTUREONLY"));
}

/// A resumed session is rendered from the record after its last consumed `records`.
#[test]
fn ac3_resumed_session_renders_only_new_records() {
    let mut h = home(Some("resumed"));
    let inputs = h.dir.parent().unwrap().join("inputs");
    h.set("STUB_INPUTS", inputs.to_string_lossy());
    h.set("STUB_EXTRACT", "none");
    h.init_files();
    h.ok(&["run"], 0);
    assert!(h.check(&expected("resumed"), RUN).is_empty());
    let extract = fs::read_to_string(
        common::walk(&inputs)
            .into_iter()
            .find(|p| p.ends_with("001.txt"))
            .unwrap(),
    )
    .unwrap();
    let transcript = extract.split("# Transcript").nth(1).unwrap();
    assert!(
        !transcript.contains("pnpm"),
        "consumed records were sent again"
    );
    assert!(transcript.contains("[8] agent:") && transcript.contains("make lint"));
}

#[test]
fn every_ac3_case_has_an_expected_file() {
    for case in CASES {
        let e = expected(case);
        assert!(e["selected"].is_u64() && e["ops"].is_object(), "{case}");
    }
}

/// A home with one scope: notes, and one pending human session.
fn one_scope(n_notes: usize) -> Home {
    let h = home(None);
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    let bodies: Vec<(String, String)> = (0..n_notes)
        .map(|i| {
            (
                format!("note-{i}"),
                format!("Fact number {i} about `tool{i}`."),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = bodies
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    notes(&h, SCOPE, &refs);
    session(
        &h,
        SCOPE,
        &uuid(1),
        &cwd,
        &["We use tool A now.", "Yes, tool A."],
    );
    h
}

fn log_lines(path: &std::path::Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// AC1: one scope per store folder that is not a worktree folder, not excluded, and has notes
/// or a mapped session.
#[test]
fn ac1_status_lists_scopes() {
    let h = home(None);
    let repo = h.dir.join("work/repo");
    git_repo(&repo);
    let wt = h.dir.join("work/repo-wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    notes(&h, "-repo", &[("a", "A.")]);
    session(&h, "-repo", &uuid(1), repo.to_str().unwrap(), &["one"]);
    notes(&h, "-repo-wt", &[("b", "B.")]);
    session(&h, "-repo-wt", &uuid(2), wt.to_str().unwrap(), &["one"]);
    notes(&h, "-excluded", &[("c", "C.")]);
    notes(&h, "-empty", &[]);
    notes(&h, "-sessions-only", &[]);
    session(&h, "-sessions-only", &uuid(3), "/nowhere/x", &["one"]);
    h.init_files();
    let mut config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(h.dir.join(".config/ai-dreamcatcher/config.json")).unwrap(),
    )
    .unwrap();
    config["exclude"] = serde_json::json!(["-excluded"]);
    fs::write(
        h.dir.join(".config/ai-dreamcatcher/config.json"),
        config.to_string(),
    )
    .unwrap();
    let out = h.ok(&["status"], 0);
    let scopes: Vec<&str> = out
        .lines()
        .skip_while(|l| *l != "scopes:")
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(scopes, ["-repo", "-sessions-only"], "{out}");
}

/// AC1 on the owner's real stores, read-only: every store folder is a scope or hidden with a
/// reason, exactly once.
#[test]
#[ignore]
fn ac1_real_stores() {
    use ai_dreamcatcher::{memory, model, sessions};
    let today = model::Time::from_system(std::time::SystemTime::now());
    let config = model::Config {
        init: today.rfc3339(),
        claude: "/nonexistent".into(),
        pond: None,
        source: "files".into(),
        exclude: Vec::new(),
        cross_provider: Vec::new(),
    };
    let stores = memory::discover();
    let n = stores.len();
    let mut map = sessions::scopes(&model::Source::Files, stores, &config, today).unwrap();
    assert_eq!(map.scopes.len(), n);
    let hidden = map.scopes.iter().filter(|s| s.hidden.is_some()).count();
    let listing = sessions::list(
        &model::Source::Files,
        today,
        &mut map,
        &model::Seen::default(),
        &config,
    )
    .unwrap();
    let listed = map
        .scopes
        .iter()
        .filter(|s| s.hidden.is_none())
        .filter(|s| {
            listing.mapped.contains(&s.store.id)
                || memory::open(&s.store).is_ok_and(|n| !n.notes.is_empty())
        })
        .count();
    println!(
        "AC1: {n} store folders, {hidden} worktree folders, {listed} scopes listed, {} window sessions parsed, {} unmapped",
        listing.sessions.len(),
        listing.unmapped.len()
    );
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn git_repo(dir: &std::path::Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

/// AC5: undo with no later edits leaves the store byte-identical to its pre-run state,
/// including a created `MEMORY.md`.
#[test]
fn ac5_undo_restores_bytes() {
    for with_index in [true, false] {
        let mut h = one_scope(2);
        if !with_index {
            fs::remove_file(h.store(SCOPE).join("MEMORY.md")).unwrap();
        }
        let template = h.dir.parent().unwrap().join("merge.json");
        fs::write(&template, r#"[
            {"op":"update","slug":"note-0","kind":null,"name":"note-0 renamed","description":"New description","body":"Fact 0, updated.","findings":["$ID0"]},
            {"op":"create","slug":"tool-a","kind":"project","name":"Tool A","description":"Which tool","body":"We use tool A.","findings":["$ID0"]}
        ]"#).unwrap();
        h.set("STUB_MERGE", format!("file:{}", template.display()));
        h.init_files();
        let before = files(&h.store(SCOPE));
        let out = h.ok(&["run"], 0);
        assert!(out.contains("2 applied"), "{out}");
        assert_ne!(files(&h.store(SCOPE)), before);
        h.set("DREAMCATCHER_NOW", "2026-11-14T13:00:00Z");
        h.ok(&["undo", RUN], 0);
        assert_eq!(
            files(&h.store(SCOPE)),
            before,
            "index present: {with_index}"
        );
    }
}

/// AC5 with the `MEMORY.md` shapes real stores have: blank lines, no trailing newline, and a
/// delete next to a blank line (F4).
#[test]
fn ac5_undo_restores_bytes_for_odd_index_shapes() {
    let mut h = one_scope(3);
    let index = h.store(SCOPE).join("MEMORY.md");
    fs::write(
        &index,
        "# Memory index\n\n- [note-0](note-0.md) \u{2014} About note-0\n\n- [note-1](note-1.md) \u{2014} About note-1\n- [note-2](note-2.md) \u{2014} About note-2",
    )
    .unwrap();
    let template = h.dir.parent().unwrap().join("merge.json");
    fs::write(&template, r#"[
        {"op":"update","slug":"note-0","kind":null,"name":"note-0","description":"About note-0","body":"Fact number 0 about `tool0`.\nFact number 1 about `tool1`.","findings":["$ID0"]},
        {"op":"delete","slug":"note-1","kind":null,"name":"","description":"","body":"","findings":[]},
        {"op":"create","slug":"tool-a","kind":"project","name":"Tool A","description":"Which tool","body":"We use tool A.","findings":["$ID0"]}
    ]"#).unwrap();
    h.set("STUB_MERGE", format!("file:{}", template.display()));
    h.init_files();
    let before = files(&h.store(SCOPE));
    assert!(h.ok(&["run"], 0).contains("3 applied"));
    h.set("DREAMCATCHER_NOW", "2026-11-14T13:00:00Z");
    h.ok(&["undo", RUN], 0);
    assert_eq!(files(&h.store(SCOPE)), before);
}

fn files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut v: Vec<_> = common::walk(dir)
        .into_iter()
        .map(|p| {
            let b = fs::read(&p).unwrap();
            (p, b)
        })
        .collect();
    v.sort();
    v
}

/// AC7: a note edited after `open` is not overwritten; the scope run aborts and its
/// sessions stay pending.
#[test]
fn ac7_edit_after_open_aborts() {
    let mut h = one_scope(1);
    let note = h.store(SCOPE).join("note-0.md");
    h.set(
        "STUB_APPEND",
        format!("{}|edited by an agent", note.display()),
    );
    let template = h.dir.parent().unwrap().join("merge.json");
    fs::write(&template, r#"[{"op":"update","slug":"note-0","kind":null,"name":"note-0","description":"About note-0","body":"Replaced.","findings":["$ID0"]}]"#).unwrap();
    h.set("STUB_MERGE", format!("file:{}", template.display()));
    h.init_files();
    h.ok(&["run"], 1);
    let text = fs::read_to_string(&note).unwrap();
    assert!(text.contains("edited by an agent") && !text.contains("Replaced."));
    let aborted = h.events("aborted", RUN);
    assert_eq!(aborted.len(), 1);
    assert_eq!(aborted[0]["reason"], "stale");
    assert!(h.events("consumed", RUN).is_empty() && h.events("done", RUN).is_empty());
    let status = h.ok(&["status"], 0);
    assert!(status.contains("pending sessions: 1"), "{status}");
}

/// AC8: a run writes only in store folders and the state folder; no session file or
/// project folder appears.
#[test]
fn ac8_writes_only_store_and_state() {
    let mut h = one_scope(1);
    h.set("STUB_MERGE", "create");
    h.init_files();
    let projects = |h: &Home| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(h.dir.join(".claude/projects"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let (before, dirs) = (h.files(), projects(&h));
    h.ok(&["run"], 0);
    let after = h.files();
    let store = h.store(SCOPE).strip_prefix(&h.dir).unwrap().to_owned();
    let state = h.state().strip_prefix(&h.dir).unwrap().to_owned();
    for (path, bytes) in &after {
        if before.iter().any(|(p, b)| p == path && b == bytes) {
            continue;
        }
        assert!(
            path.starts_with(&store) || path.starts_with(&state),
            "wrote {}",
            path.display()
        );
    }
    assert_eq!(projects(&h), dirs);
    assert!(!after.iter().any(|(p, _)| p.starts_with(".codex")));
}

/// AC9: after a kill during the write, the next locking command reverts what the run wrote,
/// leaves a note edited since, removes temp files, and the sessions are pending again.
#[test]
fn ac9_recover_after_kill_during_write() {
    let mut h = one_scope(2);
    let template = h.dir.parent().unwrap().join("merge.json");
    fs::write(&template, r#"[
        {"op":"update","slug":"note-0","kind":null,"name":"note-0","description":"About note-0","body":"Changed 0.","findings":["$ID0"]},
        {"op":"update","slug":"note-1","kind":null,"name":"note-1","description":"About note-1","body":"Changed 1.","findings":["$ID0"]}
    ]"#).unwrap();
    h.set("STUB_MERGE", format!("file:{}", template.display()));
    h.init_files();
    let store = h.store(SCOPE);
    let before = files(&store);
    h.ok(&["run"], 0);
    // The state a kill after step 3 leaves: notes written, no events of step 4, a cut line.
    let ledger = h.state().join("ledger.jsonl");
    let kept: Vec<String> = log_lines(&ledger)
        .into_iter()
        .filter(|l| {
            !l.contains(r#""ev":"applied""#)
                && !l.contains(r#""ev":"consumed""#)
                && !l.contains(r#""ev":"done""#)
        })
        .collect();
    fs::write(&ledger, kept.join("\n") + "\n{\"t\":\"cut").unwrap();
    fs::write(store.join(".note-0.md.dc-tmp"), "half").unwrap();
    // An agent edits note-1 after the run.
    let edited = fs::read_to_string(store.join("note-1.md")).unwrap() + "agent line\n";
    fs::write(store.join("note-1.md"), &edited).unwrap();
    h.set("DREAMCATCHER_NOW", "2026-11-14T12:30:00Z");
    h.ok(&["retry", "claude:none"], 0);
    let after = files(&store);
    let original0 = before
        .iter()
        .find(|(p, _)| p.ends_with("note-0.md"))
        .unwrap();
    assert!(after.contains(original0), "note-0 not reverted");
    assert_eq!(fs::read_to_string(store.join("note-1.md")).unwrap(), edited);
    assert!(!store.join(".note-0.md.dc-tmp").exists());
    assert!(fs::read_to_string(&ledger).unwrap().ends_with('\n'));
    let crash = h.events("aborted", RUN);
    assert_eq!(crash.len(), 1);
    assert_eq!(crash[0]["reason"], "crash");
    // note-1 was edited since, so its change is kept for a later undo (F1).
    assert!(
        h.state()
            .join("runs")
            .join(RUN)
            .join(SCOPE)
            .join("changes.json")
            .exists()
    );
    // N3: `log` offers that kept snapshot for undo.
    let log = h.ok(&["log"], 0);
    assert!(log.contains(&format!("dreamcatcher undo {RUN}")), "{log}");
    let status = h.ok(&["status"], 0);
    assert!(status.contains("pending sessions: 1"), "{status}");
    // A second locking command does not revert or abort that run again.
    h.ok(&["retry", "claude:none"], 0);
    assert_eq!(h.events("aborted", RUN).len(), 1);
}

/// Removes the step 4 events of `run`, as a kill before the ledger append leaves them.
fn cut_commit(h: &Home, run: &str) {
    let ledger = h.state().join("ledger.jsonl");
    let kept: Vec<String> = log_lines(&ledger)
        .into_iter()
        .filter(|l| {
            !(l.contains(&format!("\"run\":\"{run}\""))
                && ["applied", "consumed", "done", "undone"]
                    .iter()
                    .any(|ev| l.contains(&format!("\"ev\":\"{ev}\""))))
        })
        .collect();
    fs::write(&ledger, kept.join("\n") + "\n").unwrap();
}

/// AC9, a kill partway through one change (note renamed, `MEMORY.md` not yet): recover puts
/// the store back byte for byte (F1).
#[test]
fn ac9_recover_after_kill_inside_one_change() {
    let mut h = one_scope(1);
    let template = h.dir.parent().unwrap().join("merge.json");
    fs::write(&template, r#"[{"op":"update","slug":"note-0","kind":null,"name":"note-0 renamed","description":"About note-0","body":"Changed 0.","findings":["$ID0"]}]"#).unwrap();
    h.set("STUB_MERGE", format!("file:{}", template.display()));
    h.init_files();
    let store = h.store(SCOPE);
    let before = files(&store);
    let index_before = fs::read(store.join("MEMORY.md")).unwrap();
    h.ok(&["run"], 0);
    cut_commit(&h, RUN);
    fs::write(store.join("MEMORY.md"), index_before).unwrap();
    h.set("DREAMCATCHER_NOW", "2026-11-14T12:30:00Z");
    h.ok(&["retry", "claude:none"], 0);
    assert_eq!(files(&store), before);
    assert!(
        !h.state()
            .join("runs")
            .join(RUN)
            .join(SCOPE)
            .join("changes.json")
            .exists()
    );
}

/// AC9, a kill during `undo`: recover takes the half-done undo back, so the store is as the
/// run left it, and the run can be undone again.
#[test]
fn ac9_recover_after_kill_during_undo() {
    let mut h = one_scope(1);
    h.init_files();
    let store = h.store(SCOPE);
    let before = files(&store);
    h.ok(&["run"], 0);
    let after_run = files(&store);
    h.set("DREAMCATCHER_NOW", "2026-11-14T13:00:00Z");
    h.ok(&["undo", RUN], 0);
    assert_eq!(files(&store), before);
    cut_commit(&h, "20261114T130000Z");
    h.set("DREAMCATCHER_NOW", "2026-11-14T13:30:00Z");
    h.ok(&["retry", "claude:none"], 0);
    assert_eq!(files(&store), after_run);
    h.ok(&["undo", RUN], 0);
    assert_eq!(files(&store), before);
}

/// AC10: a second run while one runs exits 0 with `busy`; status and log work meanwhile.
#[test]
fn ac10_second_run_is_busy() {
    let mut h = one_scope(1);
    let log = h.dir.parent().unwrap().join("calls.log");
    h.init_files();
    h.set("STUB_SLEEP", "3");
    h.set("STUB_LOG", log.to_string_lossy());
    let mut first = h.command(&["run"]).spawn().unwrap();
    let start = std::time::Instant::now();
    while log_lines(&log).is_empty() {
        assert!(
            start.elapsed().as_secs() < 20,
            "the first run never called the model"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = h.ok(&["run"], 0);
    assert_eq!(out.trim(), "busy");
    h.ok(&["status"], 0);
    h.ok(&["log"], 0);
    assert!(first.wait().unwrap().success());
}

/// AC11: a tick with no pending session makes zero model calls.
#[test]
fn ac11_no_pending_no_calls() {
    let mut h = home(Some("sdk-driven"));
    h.init_files();
    let log = h.dir.parent().unwrap().join("calls.log");
    h.set("STUB_LOG", log.to_string_lossy());
    let out = h.ok(&["run"], 0);
    assert!(out.contains("nothing due"), "{out}");
    assert!(log_lines(&log).is_empty());
    // The session was found and judged, not missed.
    let status = h.ok(&["status"], 0);
    assert!(
        status.contains("other window sessions: 1") && status.contains("not human-driven"),
        "{status}"
    );
}

/// AC13: one scope run makes at most 13 model calls and 26 tries.
#[test]
fn ac13_call_and_try_caps() {
    for (merge, extract, want_tries) in [("cap", "auto", 12 + 2), ("create", "fail", 12 * 2)] {
        let mut h = home(None);
        let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
        notes(&h, SCOPE, &[("a", "A.")]);
        for i in 0..14 {
            session(&h, SCOPE, &uuid(i), &cwd, &["first", "second"]);
        }
        h.init_files();
        let log = h.dir.parent().unwrap().join("calls.log");
        h.set("STUB_LOG", log.to_string_lossy());
        h.set("STUB_MERGE", merge);
        h.set("STUB_EXTRACT", extract);
        h.ok(&["run"], 1);
        let tries = log_lines(&log);
        assert_eq!(tries.len(), want_tries, "{merge}/{extract}");
        assert!(tries.len() <= 26);
        let mut sessions: Vec<_> = h
            .events("attempt", RUN)
            .into_iter()
            .map(|e| e["session"].clone())
            .collect();
        sessions.dedup();
        assert_eq!(sessions.len(), 12);
    }
}

/// A failed merge logs claude's status and stderr, redacted, never its stdout (G5).
#[test]
fn merge_failure_output_carries_no_session_text() {
    let mut h = one_scope(1);
    h.set("STUB_MERGE", "fail");
    h.init_files();
    let out = h.run(&["run"]);
    assert_eq!(out.status.code(), Some(1));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("merge failed"), "{all}");
    assert!(!all.contains("PRIVATE-SESSION-TEXT"), "{all}");
    assert_eq!(h.events("aborted", RUN)[0]["reason"], "merge");
}

/// AC14: a session from a worktree or a subdirectory maps to the repository's scope; one in
/// a non-git folder maps only to its own folder's scope.
#[test]
fn ac14_worktree_and_subdirectory_mapping() {
    let h = home(None);
    let repo = h.dir.join("work/repo");
    git_repo(&repo);
    let wt = h.dir.join("work/repo-wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    fs::create_dir_all(repo.join("sub")).unwrap();
    fs::create_dir_all(h.dir.join("work/plain/x")).unwrap();
    notes(&h, "-repo", &[("a", "A.")]);
    session(&h, "-repo", &uuid(1), repo.to_str().unwrap(), &["one"]);
    session(
        &h,
        "-repo-wt",
        &uuid(2),
        wt.to_str().unwrap(),
        &["one", "two"],
    );
    session(
        &h,
        "-repo-sub",
        &uuid(3),
        repo.join("sub").to_str().unwrap(),
        &["one", "two"],
    );
    notes(&h, "-plain", &[("b", "B.")]);
    session(
        &h,
        "-plain",
        &uuid(4),
        h.dir.join("work/plain").to_str().unwrap(),
        &["one", "two"],
    );
    session(
        &h,
        "-plainx",
        &uuid(5),
        h.dir.join("work/plain/x").to_str().unwrap(),
        &["one", "two"],
    );
    h.init_files();
    let out = h.ok(&["status"], 0);
    let scope_of = |n: usize| {
        out.lines()
            .find(|l| l.contains(&uuid(n)) && l.contains("scope"))
            .map(|l| l.split_whitespace().nth(2).unwrap().to_owned())
    };
    assert_eq!(scope_of(2).as_deref(), Some("-repo"), "{out}");
    assert_eq!(scope_of(3).as_deref(), Some("-repo"), "{out}");
    assert_eq!(scope_of(4).as_deref(), Some("-plain"), "{out}");
    assert_eq!(scope_of(5), None, "{out}");
    assert!(
        out.contains(&format!("  claude:{}", uuid(5))),
        "unmapped: {out}"
    );
}

/// AC15: an index line an agent adds before a run's write, or before an undo, survives.
#[test]
fn ac15_agent_index_lines_survive() {
    let mut h = one_scope(1);
    let index = h.store(SCOPE).join("MEMORY.md");
    h.set(
        "STUB_APPEND",
        format!(
            "{}|- [Agent](agent.md) \u{2014} added during the run",
            index.display()
        ),
    );
    h.init_files();
    h.ok(&["run"], 0);
    let text = fs::read_to_string(&index).unwrap();
    assert!(
        text.contains("added during the run") && text.contains("(stub-fact.md)"),
        "{text}"
    );
    fs::write(
        &index,
        text + "- [Later](later.md) \u{2014} added before the undo\n",
    )
    .unwrap();
    h.set("DREAMCATCHER_NOW", "2026-11-14T13:00:00Z");
    h.ok(&["undo", RUN], 0);
    let text = fs::read_to_string(&index).unwrap();
    assert!(text.contains("added during the run") && text.contains("added before the undo"));
    assert!(!text.contains("(stub-fact.md)"));
}

/// AC17: `log` lists every applied change with op, note, sources and run id, and prints the
/// undo command while that run's `changes.json` exists.
#[test]
fn ac17_log_lists_changes_and_undo_command() {
    let mut h = one_scope(1);
    h.init_files();
    h.ok(&["run"], 0);
    let out = h.ok(&["log"], 0);
    assert!(out.contains(&format!("run {RUN}")), "{out}");
    assert!(
        out.contains("Create stub-fact") && out.contains(&format!("sources: claude:{}#1", uuid(1))),
        "{out}"
    );
    assert!(out.contains(&format!("dreamcatcher undo {RUN}")), "{out}");
    // 31 days later the snapshot is gone and so is the undo command.
    h.set("DREAMCATCHER_NOW", "2026-12-15T12:00:00Z");
    h.ok(&["run"], 0);
    let out = h.ok(&["log", RUN], 0);
    assert!(
        out.contains("Create stub-fact") && !out.contains("dreamcatcher undo"),
        "{out}"
    );
}

/// AC18: a session whose extract crashes the process 3 times is not tried a fourth time,
/// until `retry`.
#[test]
fn ac18_three_crashes_give_up() {
    let mut h = one_scope(1);
    h.init_files();
    let log = h.dir.parent().unwrap().join("calls.log");
    h.set("STUB_LOG", log.to_string_lossy());
    h.set("STUB_SLEEP", "30");
    for i in 0..3 {
        h.set("DREAMCATCHER_NOW", format!("2026-11-14T12:0{i}:00Z"));
        let mut child = h.command(&["run"]).spawn().unwrap();
        let start = std::time::Instant::now();
        while log_lines(&log).len() < i + 1 {
            assert!(start.elapsed().as_secs() < 20, "no extract call");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        child.kill().unwrap();
        child.wait().unwrap();
    }
    h.set("STUB_SLEEP", "0");
    h.set("DREAMCATCHER_NOW", "2026-11-14T12:05:00Z");
    let out = h.ok(&["run"], 0);
    assert!(out.contains("nothing due"), "{out}");
    assert_eq!(log_lines(&log).len(), 3);
    assert!(h.ok(&["status"], 0).contains("given-up sessions: 1"));
    h.ok(&["retry", &format!("claude:{}", uuid(1))], 0);
    h.ok(&["run"], 0);
    assert!(log_lines(&log).len() > 3);
}

/// AC19: a create that would push `MEMORY.md` past 200 lines is dropped and shown in `log`
/// and `status`.
#[test]
fn ac19_full_index_drops_create() {
    let h = one_scope(1);
    let index = h.store(SCOPE).join("MEMORY.md");
    let mut text = fs::read_to_string(&index).unwrap();
    for i in 0..198 {
        text.push_str(&format!("- [x{i}](x{i}.md) \u{2014} filler\n"));
    }
    fs::write(&index, &text).unwrap();
    h.init_files();
    h.ok(&["run"], 0);
    assert_eq!(fs::read_to_string(&index).unwrap(), text);
    assert!(!h.store(SCOPE).join("stub-fact.md").exists());
    assert!(
        h.ok(&["log"], 0)
            .contains("dropped Create stub-fact: index_full")
    );
    assert!(h.ok(&["status"], 0).contains("index_full"));
}

/// A home whose source is the stub pond, serving the home's own session files.
fn pond_home(sessions: serde_json::Value, search: serde_json::Value) -> Home {
    let mut h = home(None);
    h.stub("pond");
    let storage = h.dir.parent().unwrap().join("pond");
    fs::create_dir_all(&storage).unwrap();
    fs::write(
        storage.join("pond.json"),
        serde_json::json!({"sessions": sessions, "search": search}).to_string(),
    )
    .unwrap();
    h.set("POND_STORAGE_PATH", storage.to_string_lossy());
    h.set(
        "POND_CONFIG_FILE",
        storage.join("config.toml").to_string_lossy(),
    );
    h
}

fn pond_session(id: &str, project: &str, file: &std::path::Path) -> serde_json::Value {
    serde_json::json!({"session_id": id, "source_agent": "claude-code", "project": project,
        "project_dir": SCOPE, "file": file})
}

/// AC20 and AC21: a pond search hit contradicts an old note no pending session mentions;
/// the note is updated and its `Sources:` line names the hit's session. Re-verify adds at
/// most 5 searches, 30,000 characters and no model call.
#[test]
fn ac20_ac21_reverify_updates_from_a_hit() {
    let mut h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    let ns: Vec<(String, String)> = (0..7)
        .map(|i| {
            (
                format!("note-{i}"),
                format!("Old fact {i} about `tool{i}`."),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = ns.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    notes(&h, SCOPE, &refs);
    let pending = session(
        &h,
        SCOPE,
        &uuid(1),
        &cwd,
        &["Nothing about notes.", "Really nothing."],
    );
    // Three hits per search, each over 2,000 characters: 5 notes reach the 30,000 budget.
    let big = "x ".repeat(5_000);
    let mut rows = vec![pond_session(&uuid(1), &cwd, &pending)];
    let mut search = Vec::new();
    for i in 2..=4 {
        let file = session(&h, SCOPE, &uuid(i), &cwd, &["note-0 is wrong now"]);
        rows.push(pond_session(&uuid(i), &cwd, &file));
        search.push(serde_json::json!({"session_id": uuid(i), "project": cwd, "source_agent": "claude-code",
            "matches": [{"message_id": "m", "role": "user", "timestamp": "2026-11-14T09:01:00.000Z", "text": big, "score": 1.0}]}));
    }
    search.push(serde_json::json!({"session_id": uuid(9), "project": cwd, "source_agent": "claude-code", "matches": [{"text": "past the limit"}]}));
    let storage = h.dir.parent().unwrap().join("pond");
    fs::write(
        storage.join("pond.json"),
        serde_json::json!({"sessions": rows, "search": search}).to_string(),
    )
    .unwrap();
    let template = h.dir.parent().unwrap().join("merge.json");
    fs::write(&template, r#"[{"op":"update","slug":"note-0","kind":null,"name":"note-0","description":"About note-0","body":"New fact 0, from a later session.","findings":["$ID0"]}]"#).unwrap();
    h.set("STUB_MERGE", format!("file:{}", template.display()));
    h.set("STUB_EXTRACT", "none");
    let inputs = h.dir.parent().unwrap().join("inputs");
    h.set("STUB_INPUTS", inputs.to_string_lossy());
    h.ok(&["init", "--source", "pond"], 0);
    let log = h.dir.parent().unwrap().join("calls.log");
    h.set("STUB_LOG", log.to_string_lossy());
    fs::write(storage.join("calls.log"), "").unwrap();
    h.ok(&["run"], 0);
    let note = fs::read_to_string(h.store(SCOPE).join("note-0.md")).unwrap();
    assert!(
        note.contains("New fact 0")
            && note.contains(&format!("Sources: claude:{} 2026-11-14", uuid(2))),
        "{note}"
    );
    let searches = log_lines(&storage.join("calls.log"))
        .iter()
        .filter(|l| *l == "search")
        .count();
    assert_eq!(searches, 5);
    assert_eq!(log_lines(&log).len(), 2, "one extract and one merge");
    let merge_input = fs::read_to_string(common::walk(&inputs).into_iter().max().unwrap()).unwrap();
    let hit_chars: usize = merge_input
        .lines()
        .filter(|l| l.starts_with("  claim: "))
        .map(|l| l.len() - 9)
        .sum();
    assert_eq!(hit_chars, 30_000);
    assert!(!merge_input.contains("past the limit"));
    assert_eq!(h.events("verified", RUN).len(), 5);
    let searches: Vec<serde_json::Value> = log_lines(&storage.join("search.log"))
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for (i, q) in searches.iter().enumerate() {
        assert_eq!(
            (&q["project"], &q["from"]),
            (&cwd.as_str().into(), &"2026-11-14".into())
        );
        assert_eq!(q["query"], format!("note-{i} tool{i}"));
    }
}

/// Re-verify keeps only top-level hits that map to this scope, Codex ones only when the scope
/// opted in.
#[test]
fn reverify_filters_hits() {
    let mut h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    notes(&h, SCOPE, &[("a", "A fact about `tool`.")]);
    let pending = session(&h, SCOPE, &uuid(1), &cwd, &["one", "two"]);
    let other = session(&h, "-other", &uuid(2), "/elsewhere", &["x"]);
    let mut other_row = pond_session(&uuid(2), "/elsewhere", &other);
    other_row["project_dir"] = "-other".into();
    let hit = |id: usize, agent: &str, project: &str| serde_json::json!({"session_id": uuid(id), "project": project, "source_agent": agent, "matches": [{"text": "hit"}]});
    let storage = h.dir.parent().unwrap().join("pond");
    fs::write(storage.join("pond.json"), serde_json::json!({
        "sessions": [pond_session(&uuid(1), &cwd, &pending), other_row],
        "search": [hit(3, "claude-code/explore", &cwd), hit(2, "claude-code", "/elsewhere"), hit(4, "codex-cli", &cwd)]}).to_string()).unwrap();
    h.set("STUB_EXTRACT", "none");
    h.ok(&["init", "--source", "pond"], 0);
    let log = h.dir.parent().unwrap().join("calls.log");
    h.set("STUB_LOG", log.to_string_lossy());
    let inputs = h.dir.parent().unwrap().join("inputs");
    h.set("STUB_INPUTS", inputs.to_string_lossy());
    h.ok(&["run"], 0);
    assert_eq!(h.events("verified", RUN)[0]["hits"], 0);
    let merge = fs::read_to_string(inputs.join("001.txt")).unwrap();
    assert!(!merge.contains("#v"), "no hit reaches merge");
}

/// A scope run whose extracts found nothing still calls merge once, so duplicate notes are
/// consolidated. The update cites no finding and keeps the union of both notes' `Sources:`.
#[test]
fn merge_runs_without_findings_and_dedups_with_merged_sources() {
    let mut h = one_scope(0);
    let store = h.store(SCOPE);
    let note = |name: &str, source: &str| {
        format!(
            "---\nname: {name}\ndescription: How to deploy\nmetadata:\n  type: project\n---\n\nDeploy with `make ship`.\nSources: {source}\n"
        )
    };
    fs::write(
        store.join("deploy-steps.md"),
        note("Deploy steps", "claude:aaa 2026-11-01"),
    )
    .unwrap();
    // The duplicate already says "from main", so the update copies that line from it.
    fs::write(
        store.join("deploying.md"),
        note("Deploying", "claude:bbb 2026-11-03")
            .replace("`make ship`.", "`make ship` from main."),
    )
    .unwrap();
    h.age_files(2 * 3_600);
    template(
        &mut h,
        r#"[
        {"op":"update","slug":"deploy-steps","kind":null,"name":"Deploy steps","description":"How to deploy","body":"Deploy with `make ship`.\nDeploy with `make ship` from main.","findings":[]},
        {"op":"delete","slug":"deploying","kind":null,"name":"","description":"","body":"","findings":[]}
    ]"#,
    );
    h.set("STUB_EXTRACT", "none");
    h.init_files();
    let log = h.dir.parent().unwrap().join("calls.log");
    h.set("STUB_LOG", log.to_string_lossy());
    assert!(h.ok(&["run"], 0).contains("2 applied"));
    let models: Vec<String> = log_lines(&log)
        .iter()
        .map(|l| l.split(' ').next().unwrap().to_owned())
        .collect();
    assert_eq!(models, ["sonnet", "opus"]);
    assert!(!store.join("deploying.md").exists());
    let kept = fs::read_to_string(store.join("deploy-steps.md")).unwrap();
    assert!(
        kept.ends_with("from main.\nSources: claude:bbb 2026-11-03, claude:aaa 2026-11-01\n"),
        "{kept}"
    );
    let applied = h.events("applied", RUN);
    assert!(
        applied
            .iter()
            .all(|e| e["sources"] == serde_json::json!([])),
        "{applied:?}"
    );
}

/// AC22: with pond failing, a tick exits 1, appends one `aborted` with reason `source`, gives
/// no strike, and `status` shows it.
#[test]
fn ac22_pond_down_aborts_tick() {
    let h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    notes(&h, SCOPE, &[("a", "A.")]);
    let file = session(&h, SCOPE, &uuid(1), &cwd, &["one", "two"]);
    let storage = h.dir.parent().unwrap().join("pond");
    let data = |fail: bool| {
        serde_json::json!({"fail": fail, "sessions": [pond_session(&uuid(1), &cwd, &file)]})
            .to_string()
    };
    fs::write(storage.join("pond.json"), data(false)).unwrap();
    h.ok(&["init", "--source", "pond"], 0);
    fs::write(storage.join("pond.json"), data(true)).unwrap();
    h.ok(&["run"], 1);
    let events: Vec<serde_json::Value> = h.ledger();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        (&events[0]["ev"], &events[0]["scope"], &events[0]["reason"]),
        (&"aborted".into(), &"*".into(), &"source".into())
    );
    let out = h.run(&["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("last failure {NOW} in run {RUN}")),
        "{stdout}"
    );
}

/// AC23: the same session through `Files` and through the stub pond gives equal `Session`
/// and `Transcript` values.
#[test]
fn ac23_files_and_stub_pond_agree() {
    use ai_dreamcatcher::model::{Agent, Source};
    use ai_dreamcatcher::sessions::{parse, read, session as parsed, transcript};
    for (case, agent, id, rel) in [
        (
            "compacted",
            Agent::Claude,
            "00000009-aaaa-4bbb-8ccc-000000000009",
            ".claude/projects/-fixture-work-app/00000009-aaaa-4bbb-8ccc-000000000009.jsonl",
        ),
        (
            "codex-opted-in",
            Agent::Codex,
            "0000000a-aaaa-7bbb-8ccc-00000000000a",
            ".codex/sessions/2026/11/14/rollout-2026-11-14T09-00-00-0000000a-aaaa-7bbb-8ccc-00000000000a.jsonl",
        ),
    ] {
        let h = home(Some(case));
        let file = h.dir.join(rel);
        let storage = h.dir.parent().unwrap().join("pond");
        fs::create_dir_all(&storage).unwrap();
        let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
        let row = serde_json::json!({"session_id": id, "source_agent": if agent == Agent::Claude {"claude-code"} else {"codex-cli"},
            "project": cwd, "project_dir": SCOPE, "source": "cli", "originator": "codex-tui", "file": file});
        fs::write(
            storage.join("pond.json"),
            serde_json::json!({"sessions": [row]}).to_string(),
        )
        .unwrap();
        // pond fixtures set POND_STORAGE_PATH and POND_CONFIG_FILE and leave HOME alone.
        h.script(
            "pond-env",
            &format!(
                "#!/bin/sh\nPOND_STORAGE_PATH='{}' POND_CONFIG_FILE='{}' exec '{}' \"$@\"\n",
                storage.display(),
                storage.join("config.toml").display(),
                common::repo().join("tests/stubs/pond").display()
            ),
        );
        let pond = Source::Pond(h.bin.join("pond-env"));
        let files = parse(&read(&Source::Files, agent, id, Some(&file)).unwrap()).unwrap();
        let ponds = parse(&read(&pond, agent, id, None).unwrap()).unwrap();
        let (a, b) = (
            parsed(agent, id, Some(file.clone()), &files),
            parsed(agent, id, None, &ponds),
        );
        assert_eq!(
            (
                &a.reference,
                a.records,
                &a.start_cwd,
                &a.last,
                a.human_typed,
                a.top_level
            ),
            (
                &b.reference,
                b.records,
                &b.start_cwd,
                &b.last,
                b.human_typed,
                b.top_level
            ),
            "{case}"
        );
        assert_eq!(
            transcript(&a, &files, 0),
            transcript(&b, &ponds, 0),
            "{case}"
        );
        assert!(!transcript(&a, &files, 0).messages.is_empty());
    }
}

/// AC16: dependencies are only serde and serde_json, and `exec::run` starts only `claude`,
/// `git`, `pmset` and `pond`.
#[test]
fn ac16_dependencies_and_programs() {
    let manifest = fs::read_to_string(common::repo().join("Cargo.toml")).unwrap();
    let deps: Vec<&str> = manifest
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with('['))
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split('=').next().unwrap().trim())
        .collect();
    assert_eq!(deps, ["serde", "serde_json"]);
    let mut programs = Vec::new();
    for file in [
        "dream.rs",
        "exec.rs",
        "main.rs",
        "sessions.rs",
        "journal.rs",
        "memory.rs",
    ] {
        let text = fs::read_to_string(common::repo().join("src").join(file)).unwrap();
        let code = text.split("#[cfg(test)]").next().unwrap();
        for (i, _) in code.match_indices("Command::new(") {
            let arg = &code[i + 13..];
            programs.push(arg[..arg.find(')').unwrap()].to_owned());
        }
    }
    programs.sort();
    programs.dedup();
    // `bin` is the pond path from config.json, `&b.claude` the claude path, and `/bin/kill`
    // ends a timed-out process group.
    assert_eq!(
        programs,
        ["\"/bin/kill\"", "\"git\"", "\"pmset\"", "&b.claude", "bin"]
    );
}

/// AC3, live: every case 3 times against the real `claude`. Pass: every round passes at
/// least 10 of 11 cases, and the secret case passes always. Run it yourself with
/// `cargo test --features live --test acceptance ac3_live -- --nocapture`.
/// The tool sees the fixture home; the `claude` child gets the real `HOME` (Q16) through a
/// wrapper on `PATH`.
#[cfg(feature = "live")]
#[test]
fn ac3_live() {
    let real_home = std::env::var("HOME").unwrap();
    let real_path = std::env::var("PATH").unwrap();
    // `claude` finds its Keychain login through USER, which `Home::run` clears.
    let user = std::env::var("USER").unwrap();
    let claude = std::env::split_paths(&real_path)
        .chain([std::path::PathBuf::from(&real_home).join(".local/bin")])
        .map(|d| d.join("claude"))
        .find(|p| p.is_file())
        .expect("claude on PATH");
    // `DREAMCATCHER_CASES=a,b` runs only those cases; then every one of them must pass.
    let only = std::env::var("DREAMCATCHER_CASES").unwrap_or_default();
    let cases: Vec<&str> = CASES
        .into_iter()
        .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
        .collect();
    let need = if only.is_empty() { 10 } else { cases.len() };
    let mut rounds = Vec::new();
    for round in 1..=3 {
        let mut passed = 0;
        let mut secret_ok = true;
        for case in cases.iter().copied() {
            let mut h = Home::new(Some(case));
            h.stub("pmset");
            h.script(
                "claude",
                &format!(
                    "#!/bin/sh\nexport HOME='{real_home}' PATH='{real_path}' USER='{user}' LOGNAME='{user}'\nexec '{}' \"$@\"\n",
                    claude.display()
                ),
            );
            h.set("DREAMCATCHER_RETRY_DELAY", "30");
            let init = h.run(&["init", "--source", "files"]);
            let run = h.run(&["run"]);
            let mut fails = h.check(&expected(case), RUN);
            if !init.status.success() {
                fails.push("init failed".into());
            }
            if run.status.code() == Some(2) {
                fails.push("run: usage error".into());
            }
            println!(
                "round {round}  {case:20}  {}",
                if fails.is_empty() {
                    "pass".to_owned()
                } else {
                    fails.join("; ")
                }
            );
            passed += usize::from(fails.is_empty());
            if case == "secret" && !fails.is_empty() {
                secret_ok = false;
            }
        }
        println!("round {round}: {passed} of {} passed", cases.len());
        rounds.push((passed, secret_ok));
    }
    assert!(rounds.iter().all(|(p, s)| *p >= need && *s), "{rounds:?}");
}

/// `init` prints the launchd entry and installs nothing; `smoke` fails on leaking context or
/// a left session file; a tick exits 0 on battery; `--dry-run` writes nothing.
#[test]
fn init_smoke_battery_and_dry_run() {
    let mut h = one_scope(1);
    let out = h.ok(&["init", "--source", "files"], 0);
    assert!(
        out.contains("<plist")
            && out.contains("StartCalendarInterval")
            && out.contains("smoke: ok")
    );
    assert!(!h.dir.join("Library/LaunchAgents").exists());
    let mode = fs::metadata(h.state()).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o700
    );

    h.set("STUB_TOKENS", "5000");
    assert!(String::from_utf8_lossy(&h.run(&["smoke"]).stderr).contains("input tokens"));
    h.set("STUB_TOKENS", "100");
    let call = ai_dreamcatcher::dream::project_folder(&h.state().join("call"));
    h.set(
        "STUB_TOUCH",
        h.dir
            .join(".claude/projects")
            .join(call)
            .join("s.jsonl")
            .to_string_lossy(),
    );
    assert_eq!(h.run(&["smoke"]).status.code(), Some(1));
    h.set("STUB_TOUCH", "");

    h.set("STUB_BATTERY", "1");
    assert_eq!(h.ok(&["run"], 0).trim(), "on battery");
    h.set("STUB_BATTERY", "");
    let before = h.files();
    let out = h.ok(&["run", "--dry-run"], 0);
    assert!(
        out.contains("dry run") && out.contains("Create stub-fact"),
        "{out}"
    );
    let changed: Vec<_> = h
        .files()
        .into_iter()
        .filter(|f| !before.contains(f))
        .map(|f| f.0)
        .collect();
    assert!(
        changed
            .iter()
            .all(|p| p.starts_with(".local/share/ai-dreamcatcher/call") || p.ends_with("lock")),
        "{changed:?}"
    );

    let log = h.state().join("launchd.log");
    fs::write(&log, vec![b'x'; 2 << 20]).unwrap();
    h.ok(&["run"], 0);
    assert!(fs::metadata(&log).unwrap().len() < 1 << 20);
}

fn template(h: &mut Home, json: &str) {
    let path = h.dir.parent().unwrap().join("merge.json");
    fs::write(&path, json).unwrap();
    h.set("STUB_MERGE", format!("file:{}", path.display()));
}

/// AC19, the 25,000-byte limit: a create, or an update whose index line grows, that would
/// push `MEMORY.md` past it is dropped (O4).
#[test]
fn ac19_index_byte_limit_drops_create_and_longer_line() {
    for update in [false, true] {
        let mut h = one_scope(1);
        let index = h.store(SCOPE).join("MEMORY.md");
        let mut text = fs::read_to_string(&index).unwrap();
        let filler = format!("- [f](f.md) \u{2014} {}\n", "x".repeat(400));
        while text.len() + filler.len() < 24_990 {
            text.push_str(&filler);
        }
        // Up to 10 bytes under the limit: room for neither a new line nor a longer one.
        text.push_str(&format!("{}\n", "y".repeat(24_990 - text.len() - 1)));
        fs::write(&index, &text).unwrap();
        if update {
            template(
                &mut h,
                r#"[{"op":"update","slug":"note-0","kind":null,"name":"note-0","description":"A much longer description than the one it had, long enough to cross the limit","body":"Fact number 0 about `tool0`.","findings":["$ID0"]}]"#,
            );
        }
        h.init_files();
        h.ok(&["run"], 0);
        assert_eq!(
            fs::read_to_string(&index).unwrap(),
            text,
            "update: {update}"
        );
        let slug = if update { "note-0" } else { "stub-fact" };
        assert!(
            h.ok(&["log"], 0).contains(&format!("{slug}: index_full")),
            "update: {update}"
        );
    }
}

/// AC11 and the sdk-driven case: the session is listed, and not pending.
#[test]
fn ac11_sdk_session_is_listed_but_not_pending() {
    let h = home(Some("sdk-driven"));
    h.init_files();
    let out = h.ok(&["status"], 0);
    assert!(out.contains("pending sessions: 0"), "{out}");
    assert!(
        out.contains(
            "claude:00000013-aaaa-4bbb-8ccc-000000000013  scope -fixture-work-app  not human-driven"
        ),
        "{out}"
    );
}

/// D13: auto-dream on, or absent with a recent `.consolidate-lock`, skips the scope; false
/// lets it run.
#[test]
fn auto_dream_skips_the_scope() {
    for (settings, lock, due) in [
        (Some(r#"{"autoDreamEnabled": true}"#), false, false),
        (None, true, false),
        (Some(r#"{"autoDreamEnabled": false}"#), true, true),
    ] {
        let h = one_scope(1);
        if let Some(text) = settings {
            fs::write(h.dir.join(".claude/settings.json"), text).unwrap();
        }
        if lock {
            fs::write(h.store(SCOPE).join(".consolidate-lock"), "").unwrap();
            h.age_files(3_600);
        }
        h.init_files();
        let out = h.ok(&["run"], 0);
        assert_eq!(
            out.contains("nothing due"),
            !due,
            "{settings:?} {lock}: {out}"
        );
        if !due {
            assert!(h.ok(&["status"], 0).contains("skipped: auto-dream"));
        }
    }
}

/// AC14: a repository whose root is the home folder has no common dir, so it maps nothing.
#[test]
fn ac14_home_folder_repository_maps_nothing() {
    let h = home(None);
    git_repo(&h.dir);
    for d in ["a", "b"] {
        fs::create_dir_all(h.dir.join(d)).unwrap();
    }
    notes(&h, "-a", &[("x", "X.")]);
    session(
        &h,
        "-a",
        &uuid(1),
        h.dir.join("a").to_str().unwrap(),
        &["one"],
    );
    session(
        &h,
        "-b",
        &uuid(2),
        h.dir.join("b").to_str().unwrap(),
        &["one", "two"],
    );
    h.init_files();
    let out = h.ok(&["status"], 0);
    assert!(
        out.contains(&format!("unmapped sessions: 1\n  claude:{}", uuid(2))),
        "{out}"
    );
}

/// AC13, the cap case: the abort reason is `cap`, and the due clock waits 20 hours.
#[test]
fn ac13_cap_abort_moves_the_due_clock() {
    let mut h = one_scope(1);
    h.set("STUB_MERGE", "cap");
    h.init_files();
    h.ok(&["run"], 1);
    assert_eq!(h.events("aborted", RUN)[0]["reason"], "cap");
    h.set("DREAMCATCHER_NOW", "2026-11-14T13:00:00Z");
    assert!(h.ok(&["run"], 0).contains("nothing due"));
    h.set("DREAMCATCHER_NOW", "2026-11-15T08:00:00Z");
    h.set("STUB_MERGE", "create");
    assert!(h.ok(&["run"], 0).contains("1 applied"));
}

/// AC7, a note found stale in commit step 3, after an earlier change of the run was written:
/// that change is reverted and nothing of the run stays.
#[test]
fn ac7_stale_in_step_three_reverts_what_was_written() {
    let mut h = one_scope(2);
    let note1 = h.store(SCOPE).join("note-1.md");
    template(
        &mut h,
        r#"[
        {"op":"update","slug":"note-0","kind":null,"name":"note-0","description":"About note-0","body":"Changed 0.","findings":["$ID0"]},
        {"op":"update","slug":"note-1","kind":null,"name":"note-1","description":"About note-1","body":"Changed 1.","findings":["$ID0"]}
    ]"#,
    );
    h.init_files();
    let before = files(&h.store(SCOPE));
    // An agent edits note-1 while note-0 is being written: a stub `sync` cannot do that, so
    // the step-3 path is driven through the library instead.
    use ai_dreamcatcher::{memory, model};
    let notes = memory::open(&model::Store {
        id: SCOPE.into(),
        dir: h.store(SCOPE),
        auto_dream: None,
        consolidated: None,
    })
    .unwrap();
    let f = model::Finding {
        id: "claude:a#1".into(),
        slug: None,
        kind: None,
        claim: "c".into(),
        msg: Some(1),
        session: "claude:a".into(),
        date: "2026-11-14".into(),
    };
    let change = |slug: &str| model::Change {
        op: model::Op::Update,
        slug: slug.into(),
        kind: None,
        name: slug.into(),
        description: format!("About {slug}"),
        body: format!("Changed {slug}."),
        findings: vec!["claude:a#1".into()],
    };
    let plan = memory::plan(&notes, &[change("note-0"), change("note-1")], &[f]).unwrap();
    assert_eq!(
        memory::write(&h.store(SCOPE), &plan.planned[0]).unwrap(),
        memory::Written::Done
    );
    fs::write(&note1, "edited\n").unwrap();
    assert_eq!(
        memory::write(&h.store(SCOPE), &plan.planned[1]).unwrap(),
        memory::Written::Stale
    );
    assert_eq!(
        memory::revert(&h.store(SCOPE), &plan.planned[0]).unwrap(),
        memory::Written::Done
    );
    let mut expected = before.clone();
    for (p, b) in &mut expected {
        if p.ends_with("note-1.md") {
            *b = b"edited\n".to_vec();
        }
    }
    assert_eq!(files(&h.store(SCOPE)), expected);
}

/// AC22, pond failing in the middle of a scope run (render, then re-verify): the scope run
/// aborts with `source`, the tick ends, and no session gets a strike.
#[test]
fn ac22_pond_down_during_render_or_reverify() {
    for fail_on in ["messages", "search"] {
        let mut h = pond_home(serde_json::json!([]), serde_json::json!([]));
        let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
        let mut sessions = Vec::new();
        for (i, scope) in ["-first", "-second"].iter().enumerate() {
            notes(&h, scope, &[("a", "A fact about `tool`.")]);
            let file = session(
                &h,
                scope,
                &uuid(i + 1),
                &format!("{cwd}{i}"),
                &["one", "two"],
            );
            sessions.push(serde_json::json!({"session_id": uuid(i + 1), "source_agent": "claude-code", "project": format!("{cwd}{i}"), "project_dir": scope, "file": file}));
        }
        let storage = h.dir.parent().unwrap().join("pond");
        // Listing reads one record page per session; the failure comes after those.
        let data = |fail: Option<&str>| {
            serde_json::json!({"fail_on": fail, "pages_ok": 2, "sessions": sessions, "search": []})
                .to_string()
        };
        fs::write(storage.join("pond.json"), data(None)).unwrap();
        h.ok(&["init", "--source", "pond"], 0);
        fs::write(storage.join("pond.json"), data(Some(fail_on))).unwrap();
        h.set("STUB_EXTRACT", "none");
        h.ok(&["run"], 1);
        let aborted = h.events("aborted", RUN);
        assert_eq!(aborted.len(), 1, "{fail_on}: {aborted:?}");
        assert_eq!(
            (&aborted[0]["scope"], &aborted[0]["reason"]),
            (&"-first".into(), &"source".into()),
            "{fail_on}"
        );
        assert!(
            h.events("attempt", RUN)
                .iter()
                .all(|e| e["scope"] == "-first"),
            "the tick went on"
        );
        fs::write(storage.join("pond.json"), data(None)).unwrap();
        assert!(h.ok(&["status"], 0).contains("given-up sessions: 0"));
        let seen_strikes = h
            .ok(&["status"], 0)
            .contains(&format!("claude:{}  scope -first", uuid(1)));
        assert!(seen_strikes, "{fail_on}: the session must stay pending");
    }
}

/// F7: pond synced a session's later records only after a run consumed it. They are read in
/// a later run, even though the newest message is older than that run.
#[test]
fn pond_late_records_are_read_later() {
    let mut h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    notes(&h, SCOPE, &[("a", "A.")]);
    let file = session(&h, SCOPE, &uuid(1), &cwd, &["one", "two", "three"]);
    let storage = h.dir.parent().unwrap().join("pond");
    fs::write(
        storage.join("pond.json"),
        serde_json::json!({"sessions": [pond_session(&uuid(1), &cwd, &file)]}).to_string(),
    )
    .unwrap();
    // A run at 11:30 consumed records up to line 5; the session's newest message is 09:06.
    let ledger = [
        r#"{"t":"2026-11-14T11:30:00Z","ev":"attempt","run":"20261114T113000Z","scope":"SCOPE","session":"REF","records":5}"#,
        r#"{"t":"2026-11-14T11:30:00Z","ev":"extracted","run":"20261114T113000Z","scope":"SCOPE","session":"REF"}"#,
        r#"{"t":"2026-11-14T11:30:00Z","ev":"consumed","run":"20261114T113000Z","scope":"SCOPE","session":"REF","records":5}"#,
        r#"{"t":"2026-11-13T11:30:00Z","ev":"done","run":"20261114T113000Z","scope":"SCOPE"}"#,
    ]
    .join("\n")
    .replace("SCOPE", SCOPE)
    .replace("REF", &format!("claude:{}", uuid(1)));
    h.ok(&["init", "--source", "pond"], 0);
    fs::write(h.state().join("ledger.jsonl"), ledger + "\n").unwrap();
    let inputs = h.dir.parent().unwrap().join("inputs");
    h.set("STUB_INPUTS", inputs.to_string_lossy());
    h.ok(&["run"], 0);
    assert_eq!(h.events("attempt", RUN).len(), 1);
    let sent = fs::read_to_string(inputs.join("000.txt")).unwrap();
    let transcript = sent.split("# Transcript").nth(1).unwrap();
    assert!(
        transcript.contains("[6] user:") && !transcript.contains("[4] user:"),
        "{transcript}"
    );
}

/// F9: a pond command that fails after printing rows leaves no session text in the output
/// that reaches `launchd.log`.
#[test]
fn pond_failure_output_carries_no_session_text() {
    let h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    notes(&h, SCOPE, &[("a", "A.")]);
    let file = session(
        &h,
        SCOPE,
        &uuid(1),
        &cwd,
        &["PRIVATE-SESSION-TEXT one", "two"],
    );
    let storage = h.dir.parent().unwrap().join("pond");
    let data = |fail: bool| {
        serde_json::json!({"rows_then_fail": fail, "sessions": [pond_session(&uuid(1), &cwd, &file)]}).to_string()
    };
    fs::write(storage.join("pond.json"), data(false)).unwrap();
    h.ok(&["init", "--source", "pond"], 0);
    fs::write(storage.join("pond.json"), data(true)).unwrap();
    let out = h.run(&["run"]);
    assert_eq!(out.status.code(), Some(1));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("source down") && !all.contains("PRIVATE-SESSION-TEXT"),
        "{all}"
    );
}

/// F10: under pond, a session with fewer than 2 typed messages is not read in full.
#[test]
fn pond_does_not_read_sessions_that_cannot_be_pending() {
    let h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    notes(&h, SCOPE, &[("a", "A.")]);
    let file = session(&h, SCOPE, &uuid(1), &cwd, &["only one"]);
    let storage = h.dir.parent().unwrap().join("pond");
    fs::write(
        storage.join("pond.json"),
        serde_json::json!({"sessions": [pond_session(&uuid(1), &cwd, &file)], "no_ordinal": 0})
            .to_string(),
    )
    .unwrap();
    h.ok(&["init", "--source", "pond"], 0);
    fs::write(storage.join("calls.log"), "").unwrap();
    let out = h.ok(&["status"], 0);
    assert!(out.contains("fewer than 2 typed messages"), "{out}");
    assert!(
        out.contains("codex rows in pond without an ordinal (older format, never read): 0"),
        "{out}"
    );
    // scopes, list and facts: three queries, and no record page.
    assert_eq!(log_lines(&storage.join("calls.log")).len(), 3);
}

/// O2: `status` reports Codex pond rows without an ordinal.
#[test]
fn status_reports_codex_rows_without_ordinal() {
    let h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    notes(&h, SCOPE, &[("a", "A.")]);
    let file = session(&h, SCOPE, &uuid(1), &cwd, &["one"]);
    let mut row = pond_session(&uuid(1), &cwd, &file);
    row["no_ordinal"] = 7.into();
    let storage = h.dir.parent().unwrap().join("pond");
    fs::write(
        storage.join("pond.json"),
        serde_json::json!({"sessions": [row]}).to_string(),
    )
    .unwrap();
    h.ok(&["init", "--source", "pond"], 0);
    assert!(
        h.ok(&["status"], 0)
            .contains("without an ordinal (older format, never read): 7")
    );
}

/// F11: a session file with a line cut by a crash is still pending.
#[test]
fn a_cut_line_does_not_hide_a_session() {
    let h = one_scope(1);
    let file = h
        .dir
        .join(".claude/projects")
        .join(SCOPE)
        .join(format!("{}.jsonl", uuid(1)));
    let text = fs::read_to_string(&file).unwrap();
    let (first, rest) = text.split_once('\n').unwrap();
    fs::write(&file, format!("{first}\n{{\"type\":\"user\",\"cut\n{rest}")).unwrap();
    h.age_files(2 * 3_600);
    h.init_files();
    assert!(h.ok(&["status"], 0).contains("pending sessions: 1"));
}

/// F13: `log` prints the undo command only for runs that committed, not for undo runs.
#[test]
fn log_offers_undo_only_for_committed_runs() {
    let mut h = one_scope(1);
    h.init_files();
    h.ok(&["run"], 0);
    h.set("DREAMCATCHER_NOW", "2026-11-14T13:00:00Z");
    h.ok(&["undo", RUN], 0);
    let out = h.ok(&["log"], 0);
    assert!(out.contains(&format!("dreamcatcher undo {RUN}")), "{out}");
    assert!(!out.contains("dreamcatcher undo 20261114T130000Z"), "{out}");
    // N2: a second undo of the same run restores nothing.
    h.set("DREAMCATCHER_NOW", "2026-11-14T14:00:00Z");
    let out = h.ok(&["undo", RUN], 0);
    assert!(out.contains(": 0 restored"), "{out}");
}

/// F15: a store over the notes budget takes no scope slot, so 3 other due scopes all run.
#[test]
fn an_oversized_store_takes_no_slot() {
    let h = home(None);
    for (i, scope) in ["-a", "-b", "-c", "-d"].iter().enumerate() {
        let body = if i == 0 {
            "x".repeat(210_000)
        } else {
            "Small.".into()
        };
        notes(&h, scope, &[("n", body.as_str())]);
        session(&h, scope, &uuid(i + 1), &format!("/w/{i}"), &["one", "two"]);
    }
    h.init_files();
    let out = h.ok(&["run"], 0);
    assert_eq!(out.matches("1 applied").count(), 3, "{out}");
    assert!(
        h.ok(&["status"], 0)
            .contains("-a  notes 1  last done never  skipped: notes exceed the call budget")
    );
}

/// F8: a note with no search words is checked without a search, and a hand-written
/// `Sources:` date that is not YYYY-MM-DD falls back to the file date; pond rejects both.
#[test]
fn reverify_never_sends_queries_pond_rejects() {
    let mut h = pond_home(serde_json::json!([]), serde_json::json!([]));
    let cwd = h.dir.join("work/app").to_string_lossy().into_owned();
    let store = h.store(SCOPE);
    fs::create_dir_all(&store).unwrap();
    fs::write(
        store.join("blank.md"),
        "---\nname: \"\"\ndescription: d\n---\n\nNo code spans here.\n",
    )
    .unwrap();
    fs::write(
        store.join("dated.md"),
        "---\nname: Dated note\ndescription: d\n---\n\nBody.\nSources: claude:x yesterday, claude:y 2026-02-31\n",
    )
    .unwrap();
    let file = session(&h, SCOPE, &uuid(1), &cwd, &["one", "two"]);
    let storage = h.dir.parent().unwrap().join("pond");
    fs::write(
        storage.join("pond.json"),
        serde_json::json!({"sessions": [pond_session(&uuid(1), &cwd, &file)]}).to_string(),
    )
    .unwrap();
    h.ok(&["init", "--source", "pond"], 0);
    h.set("STUB_EXTRACT", "none");
    h.ok(&["run"], 0);
    let searches: Vec<serde_json::Value> = log_lines(&storage.join("search.log"))
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(searches.len(), 1, "{searches:?}");
    assert_eq!(searches[0]["query"], "Dated note");
    assert_eq!(searches[0]["project"], cwd.as_str());
    assert_eq!(searches[0]["from"], "2026-11-14");
    assert_eq!(h.events("verified", RUN).len(), 2);
}

/// N8: a run that crashed after writing some ledger lines shows only its crash and undo
/// command in `log`, not changes that `recover` reverted.
#[test]
fn log_hides_the_changes_of_a_crashed_run() {
    let mut h = one_scope(1);
    h.init_files();
    h.ok(&["run"], 0);
    // A kill inside the step-4 append: `applied` lines written, `done` not.
    let ledger = h.state().join("ledger.jsonl");
    let kept: Vec<String> = log_lines(&ledger)
        .into_iter()
        .filter(|l| !l.contains("\"ev\":\"done\""))
        .collect();
    fs::write(&ledger, kept.join("\n") + "\n").unwrap();
    h.set("DREAMCATCHER_NOW", "2026-11-14T12:30:00Z");
    h.ok(&["retry", "claude:none"], 0);
    let out = h.ok(&["log"], 0);
    assert!(
        out.contains(&format!("run {RUN}")) && out.contains("crashed during its write"),
        "{out}"
    );
    assert!(!out.contains("Create stub-fact"), "{out}");
}
