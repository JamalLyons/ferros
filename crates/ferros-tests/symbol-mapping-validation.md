# Issue #76 followup F2, attempt 1: symbol-mapping investigation

## Outcome

**Unresolved. The reported `mapped symbol` panic was not reproduced, no actual mapping cause is established, and no relocation or loader behavior is changed.** No retry or timeout increase is proposed. There is no failing-before/passing-after engine regression, so this must not be presented as an engine fix or proof that the original panic is a baseline defect.

Assigned checkout: `/Users/jamallyons/.t3/worktrees/ferros/t3-0df2f3c9-c87872176883df72`.
Branch: `t3/fix-symbol-mapping`.
Initial clean HEAD matched the requested base exactly: `60bb6e7aefba980fa6bc07d7fa71fba254f67296`.
Investigation started at approximately 2026-11-14 14:34 UTC; deadline 15:04 UTC.
No diagnostics from `687bfb8` or `b873712` were applied; the latter's report and original raw failure were read as evidence only.

## What the original panic establishes

`/tmp/ferros76-stress/full-0-0.log` shows a panic at `symbols.rs:57`, the second expect in `function_address`: `Session::runtime_address` returned `Ok(None)`. The helper had already found the executable path and a defined text symbol, and the backend's image enumeration did not return an error at this call.

The code permits three categories: the second image enumeration no longer matches the supplied path/load address; parsing that image's on-disk segment mapping failed (the current engine discards that error with `.ok()`); or the cached symbol's file virtual address is outside the parsed mapped segments. The raw failure contains insufficient data to distinguish them. A wrong slide alone does not generally explain `None`, because a valid mapping still translates a contained file address unless relocation overflows.

Read `AGENTS.md`, test README, backend README, symbol helper, single-step tests, fixture/harness, backend attach/image enumeration and portable mapping/cache code; inspected Justfile, Cargo runner/config and boundary script before running their checks. The legacy `run-test-launch.sh` contains sudo commands; none were executed. No inhibitor, sudo, push, branch switching, PR/issue mutation or other checkout edit.

## Experiments

Temporary engine instrumentation logged the original parse error for fixture images immediately before the existing `.ok()`; no such fixture error was observed. Missing shared-cache library files were observed and are already intentionally tolerated; they do not establish the cause of this executable lookup failure. A temporary helper panic captured the requested file address, initial descriptor, current loader list and direct on-disk mapping. Instrumentation did not change relocation or retry failed mappings.

An attach/map probe launched `spawn_thread` with `pause=4000`, attached, resolved `spawn_later`, read target memory, killed through Session and let harness guards clean up. Two standalone 200-iteration probes passed; further copies ran with whole suites. A corrected independent comparison probe required the exact demangled `ferros_fixture_spawn_thread::spawn_later` symbol and compared 16 executable section bytes against runtime memory for 200 launches; all passed. Its log is `/tmp/ferros76-f2-code-probe-corrected.log` (Cargo exit 0).

The first comparison prototype incorrectly used a substring match and selected a closure. That prototype failed at iteration zero; `/tmp/ferros76-f2-code-probe.log` and later probe failures in the mixed suite batch are **probe implementation errors, not debugger regressions**. It was corrected before the successful comparison run. Temporary probes and engine logging were removed from the final changes.

A 40-second filesystem sample during concurrent Cargo commands performed 24,664 full reads of `target/debug/ferros-fixture-spawn_thread`, observed no errors and exactly one SHA-256 (`f408471e10857ba8ae74e40971c2806dde20713a1de6621bc5cb706f98e9d266`, size 645,160). See `/tmp/ferros76-f2-fixture-file-sample.json`. This sample does not prove the file could never be replaced; it provides no evidence for that hypothesis.

Initial command: `cargo test -p ferros-tests --test single_step detach_releases_a_held_new_thread_and_the_process_finishes -- --nocapture`, exit 0, `/tmp/ferros76-f2-baseline.log`. Instrumentation was edited during this initial build; do not treat it as a guaranteed uninstrumented baseline run.

Stress drivers use three targeted streams plus two full streams. Each full command is `cargo test -p ferros-tests`. The first batch's targeted command is the exact original test above. Later targeted commands are `cargo test -p ferros-tests --test single_step -- --nocapture`. Per-command arguments, exit statuses and wall times are JSON lines in each summary; corresponding output is in its directory:

- `/tmp/ferros76-f2-stress.py`, summary `/tmp/ferros76-f2-stress-summary.log`, output directory `/tmp/ferros76-f2-stress/`.
- `/tmp/ferros76-f2-suite-stress.py`, summary `/tmp/ferros76-f2-suite-stress-summary.log`, output directory `/tmp/ferros76-f2-suite-stress/`. This mixed investigative batch included the temporary probe and source edits; it is not a stable final-code stress result.
- `/tmp/ferros76-f2-original-suite-stress.py`, summary `/tmp/ferros76-f2-original-suite-stress-summary.log`, output directory `/tmp/ferros76-f2-original-suite-stress/`. Original 32-test suite, no probe, failure-only helper diagnostics and temporary parse-error logging. Each stream stopped at any failure.
- `/tmp/ferros76-f2-continuous-stress.py`, summary `/tmp/ferros76-f2-continuous-stress-summary.log`, output directory `/tmp/ferros76-f2-continuous-stress/`. Original 32-test suite, no source edits during the batch. Continued past unrelated failures so they did not remove concurrent load; would stop a stream on the target mapping panic.

## Separate failures reported to lead

These are naturally observed failures with unchanged teardown code. They do not explain F2, and no files in their implementation were modified:

- `/tmp/ferros76-f2-stress/full-0-0.log`: `signal_tracing::detach_preserves_a_new_signal_racing_its_private_stop`, missing handled acknowledgement.
- `/tmp/ferros76-f2-stress/full-1-2.log`: `lifecycle::released_processes_run_to_completion`, `the detached launched process did not run to completion` at lifecycle.rs:688.
- `/tmp/ferros76-f2-suite-stress/full-0-1.log`: `lifecycle::released_processes_run_to_completion`, `drop running: the released process did not run to completion` at lifecycle.rs:681. The original release failure did recur here, without the later diagnostic snapshots.
- `/tmp/ferros76-f2-suite-stress/target-2-1.log`: `single_step::detach_cleanup_after_starting_step_releases_suspended_threads`, `attached process did not finish (explicit: true)`.
- `/tmp/ferros76-f2-original-suite-stress/target-0-2.log`: `single_step::backend_drop_releases_directly_observed_step_suspends`, `backend drop left worker threads suspended`.
- `/tmp/ferros76-f2-original-suite-stress/full-1-1.log`: another release lifecycle drop-running timeout.
- `/tmp/ferros76-f2-original-suite-stress/full-0-1.log`: `lifecycle::replacement_signals_do_not_hide_later_signal_stops`, resume refused because stop events were pending.

Additional natural failures in the sustained batch are listed below from the raw logs. Lead/F3 retain task, exception and signal teardown ownership.

## Final changes and limitations

Only the test symbol helper's failure message and this report are intended. The helper still uses Session's mapping and still panics on `None`; additional parsing/image enumeration occurs only to explain that failure, never to return a substitute address. The direct on-disk parse in that panic is a later snapshot; it cannot recover an earlier transient error already discarded by the engine. No engine mapping fix or success-path validation change is claimed. No new integration regression is retained because none failed before a demonstrated engine fix.

The remaining task is to capture an actual occurrence with the original parse error or loader/file-address mismatch, then implement and validate the corresponding fix. Thousands of successful lookups cannot retrospectively attribute the reported panic. Shared Cargo artifacts, source edits in the early exploratory batches, and independent active checkouts affect timing; no universal reliability claim follows from these samples.

## Completed stress counts

- `stress`: targeted/single-step exit 0: 36; exit 101: 0. Full suite exit 0: 2; exit 101: 2.
- `suite-stress`: targeted/single-step exit 0: 17; exit 101: 3. Full suite exit 0: 3; exit 101: 2.
- `original-suite-stress`: targeted/single-step exit 0: 34; exit 101: 1. Full suite exit 0: 2; exit 101: 2.
- `continuous-stress`: targeted/single-step exit 0: 115; exit 101: 5. Full suite exit 0: 21; exit 101: 3.

No `mapped symbol` panic or fixture-image parse-error diagnostic occurred in these batches. Exact target test passed whenever reached, including suites that failed elsewhere. The mixed exploratory batch has three full/single-step failures caused by the incorrect comparison prototype; these must not be counted as natural debugger failures. The original-suite and sustained batches had no source changes or probe.

Sustained-batch natural failures (all exit 101):

- `/tmp/ferros76-f2-continuous-stress/target-0-8.log`: `detach_after_a_step_leaves_the_process_running`.
- `/tmp/ferros76-f2-continuous-stress/target-1-10.log`: `backend_drop_releases_directly_observed_step_suspends`.
- `/tmp/ferros76-f2-continuous-stress/full-0-2.log`: `held_signal_can_be_replaced_with_a_nonfatal_signal`.
- `/tmp/ferros76-f2-continuous-stress/target-2-16.log`: `thread_created_before_pthread_is_published_stays_at_the_trap`.
- `/tmp/ferros76-f2-continuous-stress/target-2-25.log`: `detach_after_a_step_leaves_the_process_running`.
- `/tmp/ferros76-f2-continuous-stress/target-2-30.log`: `thread_created_before_pthread_is_published_stays_at_the_trap`.
- `/tmp/ferros76-f2-continuous-stress/full-0-7.log`: `detach_preserves_a_new_signal_racing_its_private_stop`.
- `/tmp/ferros76-f2-continuous-stress/full-0-10.log`: `early_replied_signal_can_be_replaced_with_sigkill`.

The sustained batch completed at 2026-11-14 14:51:32 UTC. Its final `pgrep -fl ferros-fixture-` exited 1 with no output. Earlier pgrep output included active fixtures from this and other checkouts; none were killed. Harness-owned guards performed cleanup; no external cleanup command was needed.

## Final verification

The first verification attempt found Clippy’s 50-byte `large_stack_arrays` limit in the expanded panic formatting. It was corrected by writing the diagnostic in smaller groups; no lint suppression was added. That first `just lint` exited 101 (`/tmp/ferros76-f2-final-lint.log`). Its other checks passed, but only the following complete rerun is the final-code verification:

- `just fmt-check`: exit 0, 1.509s; `/tmp/ferros76-f2-verified-fmt-check.log`.
- `just lint`: exit 0, 1.330s; `/tmp/ferros76-f2-verified-lint.log`.
- `just test`: exit 0, 2.100s; `/tmp/ferros76-f2-verified-test.log`.
- `just test-integration`: exit 0, 49.658s; `/tmp/ferros76-f2-verified-test-integration.log`.
- `just check-boundary`: exit 0, 0.071s; `/tmp/ferros76-f2-verified-check-boundary.log`.

Six final invocations of the exact reported test ran as three streams of two repetitions alongside the final integration run. All exited 0 (7.809–8.017s command wall time). Command: `cargo test -p ferros-tests --test single_step detach_releases_a_held_new_thread_and_the_process_finishes -- --nocapture`. Summary `/tmp/ferros76-f2-final-targeted-summary.log`, logs `/tmp/ferros76-f2-final-targeted/target-{0,1,2}-{0,1}.log`. Its intermediate pgrep included a still-running fixture from the integration command; it was not killed. The final verified integration/check batch’s `pgrep -fl ferros-fixture-` exited 1, with no output.

Self-reviewed the final diagnostic diff and report. Lead review remains outstanding; no delegated review was started. All raw drivers/logs remain local `/tmp` evidence. Final intended files are `crates/ferros-tests/src/symbols.rs` and this report. Working branch remains the host-assigned `t3/fix-symbol-mapping`; lead alone integrates/pushes. No engine source, single-step regression, signal test, task/exception teardown, fixture or harness changes remain. No engine implementation attempt was made without a demonstrated cause.

Verification completed 2026-11-14 14:54 UTC, inside the 30-minute limit. F2 acceptance remains unmet: no actual mapping cause, fix, or failing-before/passing-after engine regression.
