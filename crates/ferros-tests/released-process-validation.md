# Issue #76: released-process investigation (partial)

The reported timeout was not reproduced. **No root cause is established, no
release implementation is changed, and the 15-second exit timeout is unchanged.**
This report records diagnostic improvements and evidence, not a fix for #76.

## Scope

Base: `60bb6e7aefba980fa6bc07d7fa71fba254f67296` (`master`).
Environment: macOS arm64, Darwin 27.0.0, pinned Rust 1.100.0.

The lifecycle test now records attach/release and completion durations. Failures
capture both shell and fixture states (`ps`: PID, PPID, state, elapsed/CPU time,
wait channel, command), fixture progress, Mach task suspend count, per-thread
suspend counts/run states/flags, and exception ports before cleanup. Sampling is
not atomic; a concurrent exit may return errors. Independent successful samples
are preserved. The helper does not attach, suspend or resume the process.

Output goes to a regular temporary file, so it cannot fill a pipe. A file guard
removes it even if spawning or PID discovery panics; the process cleanup guard is
installed before discovery. Shell wait errors include the snapshot. No fixture,
harness ownership, toolchain, production release path or timeout is changed.

## Baseline and repeated load

Commands were run from the isolated issue checkout. Local raw logs and temporary
experiment drivers were retained outside the repository; this report preserves
the relevant results. Scripts and runner side effects were inspected first.

- At the unmodified base, one targeted run and one full suite passed. Three
  simultaneous full suites also passed (all finished within 42.07 seconds).
- First investigation: 35 natural targeted invocations and 10 full suites;
  all targeted runs passed, nine full suites passed. The remaining full suite
  failed at `single_step::detach_releases_a_held_new_thread_and_the_process_finishes`,
  `symbols.rs:57`, `expect("mapped symbol")`. The release lifecycle test passed.
- Independent lead batch at diagnostic head `b873712`: two streams of 20
  targeted invocations alongside three streams of up to two full suites each
  (a stream stops on failure). All 40 targeted commands passed. Four of five
  full suites passed; one failed in
  `signal_tracing::dropping_a_running_backend_with_a_pending_stop_releases_the_target_promptly`,
  `signal_tracing.rs:31`, `missing handled acknowledgement`.
- Thus 75 natural targeted invocations passed, and 13 of 15 full suites passed.
  Neither full-suite failure explains the original lifecycle timeout. Both
  different loaded failures remain untracked pending follow-up authorization.

Targeted command:
`cargo test -p ferros-tests --test lifecycle released_processes -- --nocapture`.
Full-suite command: `cargo test -p ferros-tests`.

The first diagnostic stress batch used three streams of ten targeted commands
alongside two streams of up to three full suites. All 180 attached release cases
passed: attach/release took 0.043876416–0.191110291 seconds, and completion took
1.017451125–1.239478208 seconds after release. All 30 launched-detach cases passed.
Build locks may affect command wall times; test processes run concurrently after
Cargo releases its build lock. These samples do not reproduce an exact scheduler
ordering and do not justify enlarging a 15-second timeout.

## Experiments and limits

A temporary explicit SIGSTOP immediately before the completion wait validated the
original diagnostic failure path: exit 101 after a 15.302884666-second wait,
shell state `S`, fixture state `T`, Mach counts `(1, [0])`, and partial tick output.
The injected stop was removed. This validates diagnostics, not the cause of #76.

A separate lead experiment temporarily reduced `DETACH_STOP_TIMEOUT` from one
second to zero for five targeted runs. All five passed. The original constant
was restored. Even that proxy did not demonstrate a SIGCONT ordering failure;
these runs are excluded from the natural repetition count above.

The existing detach code waits at most one second for its replied private SIGSTOP
to become BSD-visible before sending SIGCONT. A late stop is a plausible
hypothesis, but no observation established that path. The shell completion wait
also permits parent/reaping delay or slow fixture progress as explanations.
Zero Mach counts alone cannot establish where a thread is blocked, and a BSD
job-control stop may itself contribute to the task suspend count.

## Acceptance

- **C1 unmet:** no natural timeout snapshot or demonstrated root cause. Preserve
  the diagnostics for the next occurrence; no evidence supports a backend fix
  or timeout adjustment yet.
- **C2 observed sampling:** repeated targeted tests passed alongside full suites.
  The two separate full-suite failures limit broader reliability conclusions.
- **C3 conditional:** no backend release behavior changed, so there is no claimed
  failing-before/passing-after engine regression.

## Verification and review

The implementation agent ran all five required checks successfully on `b873712`.
Two fresh read-only reviews (Codex and Claude) found no blocker. Lead corrections
address their file cleanup, partial samples, scheduling-state/exception-port
coverage, timing-label and machine-specific-report findings.

Final corrected code: `just fmt-check`, `just lint`, `just test`,
`just test-integration`, and `just check-boundary` all exited 0. The expanded
snapshot was then exercised with the temporary deliberate-stop experiment:
exit 101 as intended, task suspend count 1, thread suspend count 0, run_state 3,
flags 1, and exception-port entries captured. The injected stop was removed.
This is failure-path validation, not reproduction of #76. A final targeted run
on the restored source passed, followed by format and boundary checks.

Leak probes distinguish fixtures in active unrelated checkouts from this run's
processes. The completed stress batch's final probe was empty; later probes
found only active fixtures in another checkout, with none owned by this run.
