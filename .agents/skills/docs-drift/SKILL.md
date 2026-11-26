---
name: docs-drift
description: >
  Sweep Ferros documentation against the code, tests, crate layout, and
  GitHub issue state, then update stale docs in place. Use when someone
  invokes /docs-drift, notices documentation drift, or asks to refresh
  README, ROADMAP, AGENTS.md, rustdoc, CLI help, website claims, or issue
  references that no longer match the project.
---

# docs-drift

One invocation: `/docs-drift`.

Refresh project documentation that has drifted from the code or from GitHub. Edit the docs. Do not change debugger behavior, and do not change GitHub issues unless the person who invoked the skill explicitly asks.

## Evidence first

Run the evidence script before editing anything. It only prints facts. It does not modify files, and a non-zero pattern count is not a failure.

```sh
just docs-drift
```

If `just` is not installed, run `sh scripts/docs-drift.sh` from the repository root. Reason from that output and from the files it names. Do not correct a status, a command, or an issue from memory. If `gh` was unavailable, say so in the report and do not guess issue state.

The script always re-reads the phase issues (`#26`, `#40`, `#41`, `#210`) and every issue number cited by current docs. Counts in this skill are not facts. The script output is.

A pattern hit is a lead. `Phase 1.5` and `still open` still match after a correct edit. Re-read the sentence against the issue table before changing it. Do not revert a true sentence because it matched.

## Scope

Check these, and fix the ones that are wrong:

- `README.md`, `ROADMAP.md`, `CONTRIBUTING.md`
- `AGENTS.md` at the repo root and any per-crate `AGENTS.md`
- `docs/`, except the historical exclusions below
- Crate `README.md` files
- Crate-level and module rustdoc (`//!` on `lib.rs` and `mod.rs`, and `///` on public items those docs describe)
- CLI help text (`crates/ferros/src/main.rs` clap `about` and command docs)
- `website/` claims, including the roadmap section in `website/index.html`
- Doc references to GitHub issues and epics

Compare them to:

- The code. Does the API, command, flag, path, or just recipe still exist? Does the advice still match what the code and `AGENTS.md` say to do?
- GitHub issue and pull-request state. Closed versus described as open, and phase or epic text whose status is wrong.
- The crate layout in the root `Cargo.toml` (`members` and `exclude`).
- Where tests actually live (`crates/ferros-tests/tests/`, `crates/ferros/tests/`, crate unit tests).

## What not to rewrite

- `docs/reviews/` is a historical snapshot, not current guidance. Do not bring it up to date. If a current doc cites a review as if it described today's tree, fix the citation.
- One-off notes `crates/ferros-tests/*-validation.md` stay as the record of a past run unless a current doc links them as instructions.
- An accepted ADR's Context section describes the code when the decision was made. Do not modernize it. The Decision section is the owner decision. Change a status line only when it claims the present and the code or GitHub state shows that claim is false.
- `vendor/` is a submodule. Do not edit it as part of this sweep.

## Categories

Label every finding with one of these:

| Category | Meaning |
|---|---|
| stale fact | A concrete claim (command, path, crate, test, behavior) that is no longer true |
| dead reference | A link, issue, or path that does not resolve |
| wrong status | Open/closed, phase, or epic progress that disagrees with GitHub or the roadmap's own sequencing note |
| outdated advice | Instructions a contributor would follow that contradict `AGENTS.md` or the code |
| missing doc | A current fact a reader of that page needs and that page does not mention |

## Fix

1. Categorize the finding from the evidence. If the evidence does not prove it, leave the text and record it as unresolved.
2. Edit in place. Smallest change that makes the sentence true. Match the surrounding voice and heading style. Do not add a new document for the sweep.
3. Do not describe a goal as something that ships. Ownership, lifetimes, MIR, value inspection, async tasks, Linux, and Windows are goals until a test or a public command exercises them. `ferros-mir` and `ferros-protocol` are empty reserved crates. `ferros-ui` is parked and excluded from the workspace. There is no `ferros-linux` or `ferros-windows` crate.
4. Phase order comes from the planning issue the script prints (today that issue is `#41`), not from older epic text. Re-read it. Do not leave "gated on" language that the planning issue has replaced.
5. Signing advice: `task_for_pid` needs the `com.apple.security.cs.debugger` entitlement or root. The documented workflow is `just sign-debugger`. Tests are signed by `scripts/cargo-test-runner.sh`. Docs must not tell anyone to `sudo ferros attach`, and must not say tests need `sudo`. Root remains a true fallback for a one-off attach; say that only as a fallback, after the entitlement.
6. A user-facing error string is behavior, not a doc comment. Do not change it in a docs-drift pass. List it under recommendations unless the invoker explicitly asks for the behavior change.
7. Anything that needs an owner decision, or that writes GitHub state (closing an issue, editing an issue body, ticking an acceptance box), goes under recommendations. Do not do it unless the invoker explicitly asks.

## Guardrails

Doc-only. No engine, CLI, or test-behavior changes.

Respect the architecture rule in `AGENTS.md`: macOS and Mach-O code stay in `ferros-macos`; `ferros-core` stays portable (no `unsafe`, no `cfg(target_os)`, no OS crate, no object-format knowledge); platform crates do not leak into each other or into core. A doc edit that would add that knowledge to a core comment is the wrong fix. Point at the platform crate instead. A code violation discovered while reading is a recommendation, not a drive-by fix.

## Report

Put this in the pull request body. Keep it short enough to scan.

```markdown
## Drift report

Evidence: `just docs-drift` at `<rev>` on `<date>`.

### Fixed
- **category** — `path` — what changed. Why, and the evidence (file, test, or issue state).

### Recommendations
- What was left undone, why it was not an in-repo doc edit, and what the owner would do.

### Checked, no change
- Areas that were read and still matched the code or GitHub.
```

## Website claims

`website/` often states goals (MIR, ownership and borrow visualization, async inspection, interpreted execution) as present features, and its roadmap section lags `ROADMAP.md`. If an open issue says not to start that work, do not redesign the site. Record the stale sentences and the issue number as a recommendation. As of the last sweep, that issue is `#188` ("Do not start implementation yet"); confirm it is still open before following that.
