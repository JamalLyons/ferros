# Issue #76 followup F3, attempt 1 run

Date: 2026-11-14. This run used two implementation attempts, no delegates,
no pushes and no PR/issue updates.

## Checkout and scope

- Checkout: `/Users/jamallyons/.t3/worktrees/ferros/t3-cdf48f66-0bef2284fe57f2a5`.
- Initial branch: `t3/cdf48f66`; host-renamed final branch:
  `t3/fix-pending-stop-signal-ack` (the reflog records the rename at the same base).
- Expected and actual starting HEAD: `60bb6e7aefba980fa6bc07d7fa71fba254f67296`.
- No base mismatch; the checkout was initially clean.
- No diagnostics commits were cherry-picked. No common diagnostics API fields,
  symbol/image code, `single_step.rs`, or lead-owned checkout were changed.

## Observed cause of the missing signal acknowledgement

The original lead log `/tmp/ferros76-lead-stress/full-2-0.log` establishes that
backend drop returned promptly, then the fresh SIGUSR1 acknowledgement was missing.
It alone does not establish why.

The unchanged engine reproduced that exact failure with local observation:

- `/tmp/ferros76-f3-stress/target-0-60.log`, exit 101: backend-drop case;
  fixture progress was `57` before and after the two-second acknowledgement
  deadline; neither signal acknowledgement existed. An independently signed C
  Mach sampler found task suspend count **1**, both thread suspend counts **0**,
  both thread run states **3**, and BSD state **S**.
- `/tmp/ferros76-f3-sample-stress/full-0-3.log`, exit 101: another backend-drop
  failure, task count **1**, thread counts **0**. After recording the failure,
  one diagnostic SIGCONT reduced the task count to **0**, fixture progress resumed,
  and **the already-pending SIGUSR1 was handled without another SIGUSR1**.
  The test still failed. This rescue experiment is absent from the committed tests.
- `/tmp/ferros76-f3-trace-stress/full-0-0.log`, exit 101: private-stop detach
  failed to acknowledge SIGCONT. Immediately after handler shutdown the temporary
  engine trace recorded BSD `SSTOP` with physical task count **0**, while Ferros
  still counted **1** suspension. Its extra `task_resume` returned **5**.

These observations distinguish an actual task release stall from signal loss or
an acknowledgement-only filesystem timeout in the captured F3 failures.

XNU publishes BSD `SSTOP` before placing the job-control task hold. A SIGCONT in
that interval can resume nothing and leave the later hold behind after BSD state
has returned to runnable. Separately, successful PT_DETACH performs a task_resume;
Ferros previously counted that suspension again during teardown. The extra resume
can consume the newly placed job-control hold and leave the BSD observation stale.

Primary source inspected:

- https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c
  (`issignal`, `stop`, caught SIGCONT handling).
- https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/mach_process.c
  (`PT_DETACH`, `resume`).
- https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/task.c
  (`task_resume`, task basic suspend count).

The installed host was macOS 27.0.1 (26A434). Source inspection supports the native
observations; it is not a claim that the public source exactly matches that kernel.

## Changes

1. Account for the successful private-stop PT_DETACH's implicit task resume.
2. Within the existing one-second detach-stop deadline, require both BSD `SSTOP`
   and a positive physical task suspend count before sending SIGCONT.
3. Strengthen the existing native backend/Session drop regressions to require
   both the fresh SIGUSR1 and cleanup SIGCONT acknowledgements within the original
   two-second observation deadline. The fresh SIGUSR1 is still sent immediately
   after drop. Failure output records both acknowledgements, fixture progress
   before/after the wait and per-thread BSD process state.
4. Give the signal fixture a main-loop progress counter, independent of handlers.

No timeout was enlarged, no recovery signal was added to success paths, and no
failed observation is accepted as success. Baseline native drop regressions failed
before the engine change; post-change results are recorded below. The race is
naturally reproduced, not a deterministic kernel scheduling injection.

Attempt 1 added the physical-hold check. Loaded lifecycle testing exposed the
finite fixture exiting between Session detach and its second bare-backend attach
(`/tmp/ferros76-f3-lifecycle-stress/target-{0,1}-2.log`, exits 101, line 653).
Attempt 2 also accounts for PT_DETACH's implicit resume. No third engine attempt
was made.

## Other natural failures and limits

The original engine also failed
`released_processes_run_to_completion` at line 681, case `backend drop`, in
`/tmp/ferros76-f3-stress/full-2-2.log` (exit 101). It also failed the reserved
single-step backend-drop test in `full-0-0.log`. Neither had physical snapshots;
these are converging evidence, not proof of an identical cause. The lead supplied
additional original drop-running, explicit stepping-detach and signal-handshake
failures from F2; those files were read and left untouched.

A baseline load run also hit `early_replied_signal_can_be_replaced_with_sigkill`
with pending stop events on resume (`/tmp/ferros76-f3-sample-stress/full-2-1.log`).
An attempt-1 full run similarly hit `replacement_signals_do_not_hide_later_signal_stops`.
These were not investigated as part of this bounded fix.

Early validation overlapped source rebuilds and concurrent signing. Three initial
attempt-2 full streams and the first integration check died with SIGKILL before
harness tests started. `codesign --verify` confirmed an invalid harness signature.
`codesign --force` alone could not repair it; removing that generated binary's
signature and signing it again restored validation. One older stream's doctest
also referenced an rlib removed by a rebuild. None of these runs count as passes.
No runner source changes were made. Future stress runs should prebuild/sign once
and avoid source or feature rebuilds during the load window.

## Commands, exits and final validation

All commands ran in the assigned checkout, without sudo or a new inhibitor.
Each stress script writes the exact child argv, exit and duration to adjacent
JSON files and raw stdout/stderr to `.log` files. Each stream stops on failure.

- Initial baseline: `cargo test -p ferros-tests --test signal_tracing`, exit 0.
- Reproduction scripts: `python3 /tmp/ferros76-f3-stress.py`,
  `python3 /tmp/ferros76-f3-sample-stress.py`, and
  `python3 /tmp/ferros76-f3-trace-stress.py`; named child failures above exit 101.
  Later invocations in these long-lived streams used rebuilt candidates, so their
  totals are deliberately not described as baseline-only results.
- Candidate targeted smoke: `cargo test -p ferros-tests --test signal_tracing`,
  exit 0, all seven tests passed (`/tmp/ferros76-f3-attempt2.log`).
- `python3 /tmp/ferros76-f3-attempt2-stress.py`: two streams each ran 60
  `cargo test -p ferros-tests --test signal_tracing dropping_a_running_`
  invocations, all 120 exited 0 (240 native tests). Its three full streams suffered
  the invalid-signature failures described above; fresh full streams follow.
- `python3 /tmp/ferros76-f3-final-checks.py`: sequential required checks, all exit 0:
  `just fmt-check`, `just lint`, `just test`, `just test-integration`,
  `just check-boundary`. Evidence: `/tmp/ferros76-f3-final-checks/` and
  `/tmp/ferros76-f3-final-checks-summary.log`.
- `python3 /tmp/ferros76-f3-final-stress.py`: three streams requested five full
  `cargo test -p ferros-tests` runs each, alongside two streams of twenty
  `cargo test -p ferros-tests --test lifecycle released_processes_run_to_completion`
  runs each. Final results: **40/40 targeted lifecycle invocations exit 0**;
  **12/13 executed full suites exit 0**. The remaining full suite,
  `/tmp/ferros76-f3-final-stress/full-2-2.log`, exits 101 at lifecycle line 308:
  `resume to original signal` reports stop events pending. This happens before
  replacement or teardown in that test. That stream stopped there, so its last
  two requested full suites were not executed. All F3 drop and original lifecycle
  completion cases that executed in these final full runs passed.
- Stress driver processes themselves exit 0 even when a child fails; the adjacent
  per-child JSON exits above are authoritative, not the driver's status.
- The first required-check pass had lint exit 101 for diagnostic formatting's
  large stack arrays; formatting the failure details in separate messages fixed
  that, with `just lint` recheck exit 0. The first integration check and its
  pre-repair retry exit 101 for the invalid harness signature. The final sequential
  five-check run above is entirely green.
- Artifact repair commands: `codesign --verify --verbose=2 <harness binary>`
  initially exits 1; forced signing initially exits 1. After
  `codesign --remove-signature <harness binary>` and
  `codesign --entitlements crates/ferros-macos/ferros.entitlements --force --sign - <harness binary>`,
  verification exits 0. This modifies generated build output only.
- `git diff --check`, exit 0.
- After all run streams finished at 09:50, a `ps -axww -o pid=,ppid=,command=`
  scan restricted to fixture executables under this exact checkout returned
  **no owned fixture processes**, exit 0. Other checkouts' active fixtures were
  observed via `pgrep -fl ferros-fixture-` and left untouched. No fixture kill was
  needed outside the harness's own guards.

## Handoff assessment

The captured F3 missing acknowledgement has a demonstrated release-stall mechanism
and a scoped fix, with failing-before/passing-after native regression evidence.
The required checks pass. Broader loaded-suite reliability is **not fully green**:
the pending-stop resume failure remains deferred, and a common physical cause for
all original #76 / stepping failures is not established. Lead review and integration
into PR #77 remain outstanding; nothing was pushed or published by this run.
