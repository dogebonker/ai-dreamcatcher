# AGENTS.md

`docs/design.md` is the contract. Read it before changing behaviour, and change it in the same commit when behaviour changes. Its codes (G, AC, D, Q) are stable: cite them in commits and tests.

## Green

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo test -- --ignored                                        # read-only checks against your real stores and pond (AC1, AC2, AC24)
cargo test --features live --test acceptance ac3_live -- --nocapture   # live eval, real claude calls, costs money
```

`DREAMCATCHER_CASES=a,b` narrows the live eval to those fixture cases.

## Rules that are not visible in the code

- **Private data.** Sessions and memory notes are the user's. Tests and debug output print counts, paths, field names and record numbers only, never message or note text. Real-data tests install a panic hook for this reason.
- **Temp homes.** Every test runs the binary with `HOME` pointed at a temp folder (`tests/common`). The live eval hands the `claude` child the real `HOME` and `USER`, because its login lives in the Keychain.
- **Fixtures are handmade.** `tests/fixtures/<case>/` holds a fake home and `expected.json`. Never copy real sessions or notes into them.
- **Two dependencies.** `serde` and `serde_json` only, and `exec` starts only `claude`, `git`, `pmset`, `pond` and `/bin/kill` (AC16, enforced by a test).
- **Record numbers are file line numbers** under both sources: pond Codex `ordinal + 1` (D22). `records` counts only the records the parser reads (D23).
- **Every fix gets a test that fails without it.** Data-safety rules in `memory` and `journal` (stale checks, revert, undo) were each found by a reviewer reproducing a loss. Keep their tests.
