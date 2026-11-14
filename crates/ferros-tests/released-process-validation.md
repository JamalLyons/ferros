# Issue #76: released-process investigation (partial)

## Scope and outcome

Checkout: `/Users/jamallyons/.t3/worktrees/ferros/t3-fa1c224d-3f1d8808fbe72ce5`.
Branch: `t3/fix-released-process-lifecycle`.
Verified clean initial HEAD and expected master base:
`60bb6e7aefba980fa6bc07d7fa71fba254f67296`.
Investigation started 2026-11-14 14:12:42 UTC, with a 30-minute limit.
Host: macOS arm64, Darwin 27.0.0 (`genius.local`).

**The reported failure was not reproduced. No root cause is established, no
release implementation is changed, and the 15-second exit timeout is unchanged.**
This is a diagnostic handoff, not a claim that #76 is fixed.

Changes:

- The lifecycle test records attach/release and post-release completion durations.
- The shell and fixture write to a temporary file, including the existing fixture's
  `--progress` tick output. There are no output pipes to fill. Drop removes the file.
- Failures capture the shell and fixture PID, PPID, state, elapsed/CPU time, wait
  channel and command, plus output, before cleanup kills them. Shell wait errors
  are now reported instead of silently becoming exit timeouts.
- A safe diagnostics helper samples Mach task and thread suspend counts without
  attaching, suspending or resuming the target; acquired send rights are released
  on both success and error paths. Samples are not atomic; a concurrent exit can
  produce a Mach error. Zero Mach counts alone do not exclude BSD job-control stop.
- Detached launched-process timeout messages also include state, Mach counts and
  captured stdout/stderr.

## Baseline and loaded reproduction

Read `AGENTS.md`, `crates/ferros-tests/README.md`, backend README, lifecycle test,
harness, fixture, task release, exception and thread enumeration code. Inspected
`Justfile`, `.cargo/config.toml`, `scripts/cargo-test-runner.sh` and
`scripts/check-boundary.sh` before executing their tasks. No sudo or inhibitor.

At the unmodified pinned base:

1. `cargo test -p ferros-tests --test lifecycle released_processes -- --nocapture`
   exited 0; test duration 9.04 seconds. Log `/tmp/ferros76-baseline-targeted.log`.
2. `cargo test -p ferros-tests` exited 0. Log `/tmp/ferros76-baseline-full.log`.
3. Three simultaneous `cargo test -p ferros-tests` commands all exited 0;
   all finished within 42.07 seconds of their launch. Logs
   `/tmp/ferros76-base-loaded-{0,1,2}.log`. Final
   `pgrep -fl ferros-fixture-` exited 1, with no output.

With diagnostics (before final message-format-only lint fixes):

- One targeted command as above exited 0; test duration 9.12 seconds.
  Log `/tmp/ferros76-diag-targeted.log`.
- `python3 /tmp/ferros76-stress.py` ran three streams of ten consecutive targeted
  commands alongside two streams configured for up to three full suites each.
  Each stream stopped on its first failure. Exact per-command arguments, exit
  statuses, wall times and post-command pgrep output are in
  `/tmp/ferros76-stress/*.json`; test output is in adjacent `.log` files.
  Driver output: `/tmp/ferros76-stress-summary.log`.
- All 30 targeted commands exited 0. All 180 attached release cases completed;
  attach/release took 0.043876416–0.191110291 seconds and completion took
  1.017451125–1.239478208 seconds after release. All 30 launched-detach cases passed.
- Four full commands actually ran: three exited 0, one exited 101. The failure was
  `single_step::detach_releases_a_held_new_thread_and_the_process_finishes`,
  panicking at `crates/ferros-tests/src/symbols.rs:57`, `expect("mapped symbol")`.
  See `/tmp/ferros76-stress/full-0-0.log`. The release lifecycle test passed in
  that suite too. This different failure is deferred, not explained or fixed here.
- `pgrep -fl ferros-fixture-` ran after every command. During concurrency a zero
  exit included active fixtures belonging to other in-flight test processes;
  these were not killed. The batch's final pgrep exited 1 with no output. No
  external fixture cleanup was needed.

The executable Cargo runner skips signing when the executable already has its
entitlement. Tests shared this checkout's Cargo artifacts; build locks can add to
command wall times. Five simultaneous test streams are stronger load than the
original three-process report, but do not reproduce its exact scheduler ordering.

## Competing explanations inspected

- `threads()` refreshes send rights; `memory_regions()` enumerates mappings.
  Neither intentionally introduces another task suspension. This static
  observation does not prove the inspecting path cannot race.
- `detach_target` disarms thread actions, ends tracing using private SIGSTOP,
  stops the exit watcher/exception handler, balances task suspends, polls BSD
  stopped state for at most one second, sends SIGCONT, then releases rights.
  A stop arriving after that poll's deadline remains a plausible ordering; no
  observed successful release reached one second. Do not call it the cause.
- The test waits for the *shell*, not directly for the fixture. Without fixture
  output and both PPIDs/states, its old timeout could not distinguish a stopped
  target, slow target, fixture zombie, or delayed parent/reaping. Diagnostics now
  preserve those distinctions instead of assuming an insufficient timeout.
- A hundred 10-ms sleeps are scheduler dependent. The observed ~1.24-second maximum
  provides no evidence that a 15-second timeout needs enlargement.

## Diagnostic failure-path experiment

`python3 /tmp/ferros76-validate-diagnostics.py` temporarily inserted
`signal(target.pid, "-STOP");` immediately before `ran_to_completion`, after the
five initial state observations. It ran the targeted Cargo command and restored
its original source in `finally`. Cargo exited **101**, deliberately, after
16.503 seconds wall time. `/tmp/ferros76-forced-stop.log` captured:

```text
detach after inspecting: no completion after 15.302884666s
shell   PID 37926 PPID 37918 state S
fixture PID 37928 PPID 37926 state T
Mach (task suspend count, thread suspend counts): Ok((1, [0]))
fixture output: tick: 1 ... (progress before the injected stop)
```

This is validation of diagnostics, **not reproduction of #76 and not evidence of
a backend bug**. A BSD job-control stop can contribute to the task suspend count;
these counts do not identify which code owns a suspension. Final pgrep exited 1.
The injection is not present in the committed diff.

## Acceptance and remaining work

- **C1 unmet:** no natural failure, timeout snapshot or demonstrated root cause.
  No evidence justifies extending the timeout. The next recurrence should now
  preserve progress, parent and suspension evidence before cleanup.
- **C2 partial:** all release-test repetitions passed under load. Full suites
  also passed repeatedly, but one loaded full command failed in the separate
  symbol-mapping test described above; universal full-suite reliability is not
  established by this attempt.
- **C3 conditional / not applicable:** no backend release behavior was changed.
  There is no failing-before/passing-after evidence for an engine fix. The
  deliberate-stop failure above only validates diagnostic observability.

Deferred: reproduce the original failure and attribute any suspension or reaping
failure; investigate the distinct loaded single-step symbol-mapping panic. Neither
issues nor other branches were changed. No push, PR, merge or credential change.

Development checks caught and corrected a missing `collect` type annotation, then
Clippy's small configured stack-array threshold for a four-argument diagnostic
`format!`, then `format_push_string`. These were implementation/check failures,
not release failures. Final verification results follow below.

## Final verification and handoff

Three additional targeted commands ran alongside the first final full suite;
all exited 0 (9.329, 9.137 and 8.894 seconds wall time). Their logs are
`/tmp/ferros76-final-targeted-{0,1,2}.log`. The first final integration command
exited 0; Clippy then required replacing `push_str(format!(...))` with `write!`.
All five checks were rerun on the final code after that correction:

- `just fmt-check`: exit 0, 0.861 seconds; log `/tmp/ferros76-verified-fmt-check.log`.
- `just lint`: exit 0, 1.199 seconds; log `/tmp/ferros76-verified-lint.log`.
- `just test`: exit 0, 1.566 seconds; log `/tmp/ferros76-verified-test.log`.
- `just test-integration`: exit 0, 43.048 seconds; log `/tmp/ferros76-verified-test-integration.log`.
- `just check-boundary`: exit 0, 0.049 seconds; log `/tmp/ferros76-verified-check-boundary.log`.

The final integration run's `pgrep -fl ferros-fixture-` exited 1 with no output.
Earlier pgrep output during the extra targeted runs included active tests in this
checkout and active fixtures under `/Users/jamallyons/Developer/GitHub/ferros`;
none were killed. No leaked owned fixture was identified.

Total natural attempts: 35 targeted invocations and 10 full suites (9 exited 0;
1 exited 101 at the separate symbol-mapping panic). The release test passed in
all 45 invocations that included it. One additional deliberate-stop invocation
failed as designed. These passing samples do not establish the cause or fix of
a rare original failure.

Only `diagnostics.rs`, `tests/lifecycle.rs` and this report are intended for the
commit. No release mechanism, fixture implementation, timeout, toolchain, harness
ownership or unrelated test was changed. Self-reviewed the diff and cleanup paths;
lead review remains outstanding. Raw logs and temporary experiment drivers are
local `/tmp` evidence, not committed artifacts; relevant results are preserved here.

Verification completed 2026-11-14T14:21:03.810496+00:00 (under the 30-minute attempt limit).
