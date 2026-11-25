# Issue #79: resume refused with pending stop events

Date: 2026-11-14. Branch `t3/issue-79-pending-stop-resume`, base `10e3a7f`, macOS arm64.

## Captured event and ordering

The failing lifecycle tests launch `loop_counter` (stopped at entry), send
`/bin/kill -USR1` while it is stopped, then resume. A temporary probe (not
committed) did exactly that and called `backend.wait(200ms)` before any resume:

- Unchanged engine, unloaded machine: **100/100** launches published
  `Ok(Signal { signal: 30 })` before resume. So did 10/10 after a 500 ms delay at
  entry, 10/10 at an interrupt stop, and 10/10 at an interrupt stop after 300 ms.
- A temporary trace at the publish point (`run_exception_loop`, before its
  `task_suspend`) logged, per launch:
  1. the private attach stop: `Signal 17`, `suspends=0`, handshake active, internal;
  2. the client signal: `Signal 30`, `suspends=1`, `pending=false`,
     `preparing=false`, no handshake, not internal.
- With `has_pending_stop()` awaited and then `backend.resume`, the unchanged engine
  returned exactly `InvalidState("resume is not allowed while stop events are
  pending; drain them with wait")`, at entry and at an interrupt stop; the
  following `wait` returned that `Signal 30`.

Mechanism: the launch, attach and interrupt stops hold a backend `task_suspend`
without a stop of the exception loop awaiting Continue. A signal sent then is
still raised by the kernel as `EXC_SOFT_SIGNAL`, and the loop published it
immediately as a new stop (taking a second suspend). That breaks the contract
that a signal sent while stopped is not published before resume
(`single_step.rs`, `held_signal_of_a_suspended_thread_survives_a_step_of_another`).
`Session::resume` drains the queue and then calls the backend. Usually the
publication landed before that drain, so `Session::resume` returned with the
stop deferred and the tests passed. Under load it landed between the drain and
the backend's check, and the resume was refused. The engine leaked the event;
the tests do not omit a legitimate drain.

## Fix

`run_exception_loop` (`crates/ferros-macos/src/exception/run.rs`): when no stop is
pending, the backend holds suspends, no resume is being prepared and no private
handshake is active, a client soft signal is handled like one arriving during a
reported stop. It is suppressed and answered early, its logical stop is deferred,
and the loop awaits Continue. The next resume's Continue releases it, and the
loop publishes it as a stop once the task runs. Delivery, replacement and
suppression decisions then apply to it as before. Private SIGSTOPs, handshakes
and resume preparation keep their existing paths.

A deferred stop that must wait again (an interrupt or a failed resume lands
between Continue and its publication) returns to the front of the queue, so
signals keep their arrival order.

Out of scope: only soft signals are deferred. A breakpoint or step exception
received just before an interrupt is still published and refused by a resume
already preparing (the existing G8 behaviour); answering a `brk` early would raise
it again. A signal raised after a resume has begun preparing is likewise still
published and refused, since its decision cannot be taken by that resume.

## Regressions

- `lifecycle::signal_sent_while_stopped_is_reported_after_resume`: entry stop
  (suppressed and delivered), attach stop and interrupt stop. Nothing is published
  while stopped, the backend resume is accepted, exactly one `SIGUSR1` stop
  follows, delivery kills the target and suppression keeps it running without
  another stop.
- `lifecycle::signal_deferred_while_stopped_survives_teardown_and_session_resume`:
  with a signal deferred after attach, `kill` reaps the target, `detach` discards
  the decision and leaves the target running and not stopped, and
  `Session::resume` reports the deferred signal as its next stop.

Without the engine change, each of the seven cases was run on its own
(`cargo test -p ferros-tests --test lifecycle signal_sent_while_stopped` and
`... signal_deferred_while_stopped`, a temporary local edit selecting one case
per run). All seven exit 101 at `<case>: a signal sent while stopped was
published before resume`, so none passes trivially. With the change, both
tests pass.

## Commands and exits

Sequential checks, all exit 0: `just fmt-check`, `just lint`, `just test`,
`just test-integration` (14 integration binaries, 0 failures),
`just check-boundary`. Targeted suites before that, exit 0: `--test lifecycle`
(28), `--test signal_tracing` (8), `--test single_step` (32), `--test exceptions` (9).

Load batches:

1. `cargo test -p ferros-tests` streams run concurrently (two full × 3, two
   `--test lifecycle` × 15, focused signal tests × 30): 65/66 exit 0. The one
   exit 101 (`life-a-4`) was `failed to spawn fixture ... No such file or directory`
   in two tests: concurrent cargo invocations replaced fixture binaries. That is
   an artifact problem, not an engine failure, and does not count as a pass.
2. Prebuilt, signed test binaries run directly through
   `scripts/cargo-test-runner.sh`, with no cargo during the window: two streams of
   four full integration suites (14 binaries each), two streams of 20 complete
   lifecycle suites, and 40 focused runs (`signal_sent_while_stopped early_replied
   held_ replacement_signals ordinary_signals`, 8 tests): **88/88 exit 0**.

After review fixes (re-deferral order, comments, the second regression), all five
checks again exit 0 (162 integration tests). The same direct-binary batch then
ran with the second regression added to the focused filter: full suites 8/8,
lifecycle suites 39/40, focused runs 40/40. The one failure (`life-a-20`) is a
**different symptom**: `replacement_signals_do_not_hide_later_signal_stops` at
lifecycle.rs:316, `replace with a nonfatal signal: Os { code: 5, message:
"task_resume failed: KERN_FAILURE: Process not found" }`. It is not the #79
refusal. A follow-up batch ran three streams of 40 focused runs of
`replacement_signals held_ early_replied` alongside two streams of 40 complete
lifecycle suites: **120/120 exit 0** (480 replacement cases). With one
uncaptured occurrence its cause is not established: either the backend's
suspend count exceeded the kernel's, or the target died and the exit watcher
did not confirm it within resume's 200 ms window. This change routes the
deferred signal through the same suppression and publish paths as before, but
the observation is recorded as open, not classified as fixed or unrelated.

After every batch, `pgrep -fl ferros-fixture-` printed nothing (exit 1).
