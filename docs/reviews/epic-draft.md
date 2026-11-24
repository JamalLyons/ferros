# Draft epic and child issues

## Parent issue — Make debugger results reliable across target and format boundaries

**Baseline.** Trunk [7751824](https://github.com/JamalLyons/ferros/tree/7751824b6c2f5e769900675a3f0e31913a2d2b69). [ADR 0001](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/docs/adr/0001-object-file-boundary.md) and completed [#193](https://github.com/JamalLyons/ferros/issues/193) govern all children. Core owns portable policy, CPU operations and DWARF consumption. Each platform owns its OS and container code. No OS configuration switches, unsafe code or container parsing in core; no foreign-platform dependency in a platform adapter. Keep process control and object parsing at separate seams.

**Outcome.** A failed native write preserves both bytes and debugger bookkeeping. Stack traces use the right build, available unwind data and correct lookup addresses. Target-specific choices have explicit owners. The CLI and harness exercise the same contracts an additional adapter will need.

**Boundaries.** Build on [#196](https://github.com/JamalLyons/ferros/issues/196). [#195](https://github.com/JamalLyons/ferros/issues/195) owns agent efficiency. Treat [#198](https://github.com/JamalLyons/ferros/issues/198) as reserved/in flight, including its fault hooks, suspend retry and G1/G3/F6/freeze tests. Preserve [#190](https://github.com/JamalLyons/ferros/issues/190) and [#191](https://github.com/JamalLyons/ferros/issues/191). A Linux/Windows port, Phase 2, MIR and a protocol are separate product work. The UI stays parked under [#41](https://github.com/JamalLyons/ferros/issues/41).

**Parent acceptance**

- [ ] Each child below merges independently after its stated prerequisites. Each engine behavior change has a regression in `crates/ferros-tests` that fails on its predecessor. Keep existing ignored/known-broken tests and native gates.
- [ ] All listed checks pass on applicable platforms. macOS integration runs on a signed arm64 runner with fixture cleanup verified.
- [ ] ADR 0001 passes on trunk after every child. Existing source lookup, unwind, signal and cleanup behavior covered by earlier issues stays tested.

**Implementation order.** First take the completed #193 work as the baseline. Start with the guard in child 01. Numbers indicate scheduling order; only the dependencies in each body are mandatory. S = one local change, M = one coordinated behavior across a few modules, L = cross-crate interface migration with multiple adapters.

- [ ] 01 — Enforce platform ownership across workspace edges (S)
- [ ] 02 — Commit birth-trap metadata only after a successful write (M)
- [ ] 03 — Unwind an executable without source DWARF (L)
- [ ] 04 — Revalidate disk metadata when a mapped build ID becomes known (M)
- [ ] 05 — Carry each physical frame's lookup address through symbol fallback (S)
- [ ] 06 — Keep held signals until a resume actually starts their thread (M)
- [ ] 07 — Distinguish synchronous faults from deliverable signals (M)
- [ ] 08 — Let the target supply its arm64 addressing and unwind conventions (M)
- [ ] 09 — Resolve the main executable explicitly in a symbol request (M)
- [ ] 10 — Retry source discovery when its inputs change (M)
- [ ] 11 — Preserve native memory error categories at the Backend seam (M)
- [ ] 12 — Classify terminal signals at the CLI platform boundary (S)
- [ ] 13 — Separate harness supervision from platform cleanup (M)
- [ ] 14 — Give the CLI ownership of its logging worker (S)
- [ ] 15 — Make a timed-out exception continuation distinguishable from a completed one (M)

## Child 01 — Enforce platform ownership across workspace edges

**Size** S · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed), [#201](https://github.com/JamalLyons/ferros/issues/201) (closed)

**Why.** The accepted boundary is in place for current core sources. Both checkers name the same macOS crates and tokens by hand. The CLI scan recognizes only `ferros_macos`. Separately, `ferros-utils` defines its own lint tables without inheriting the workspace's unsafe prohibition. A green check gives no coverage of a future sibling-platform dependency or this lint omission. [adr/0001-object-file-boundary.md:61](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/docs/adr/0001-object-file-boundary.md#L61) · [scripts/check-boundary.sh:66](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/scripts/check-boundary.sh#L66) · [scripts/check-boundary.sh:134](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/scripts/check-boundary.sh#L134) · [tests/platform_boundary.rs:12](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/platform_boundary.rs#L12) · [ferros-utils/Cargo.toml:21](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-utils/Cargo.toml#L21) · [Cargo.toml:23](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/Cargo.toml#L23)

**Scope.** `scripts/check-boundary.sh`, `crates/ferros-tests/tests/platform_boundary.rs`, `crates/ferros-utils/Cargo.toml`. Use the existing workspace and CLI manifests as inputs.

**Design.** Use Cargo's declared package names, target predicates and dependency edges, including renamed and optional dependencies. Reject core-to-platform and platform-to-sibling edges. Check platform names in CLI source outside the factory. Run one policy from both entry points and retain the existing source-marker rules. Add the missing rust lint to utils. Keep the off-macOS checker runnable in the existing Ubuntu job and exercise the same rules with synthetic input on both hosts. Add representative ELF/PE parser identifiers to the core and wrong-platform source fixtures when enforcing those ownership rules. Neutral prose naming a supported format and portable DWARF identifiers remain valid. A textual check is a guardrail, not proof of semantic portability.

```rust
fn check_platform_edges(packages: &[Package]) -> Vec<BoundaryViolation>;
```

**Migration.** Start with synthetic dependency graphs for today's three platform names. Route both existing checks through the same predicates. Preserve the shell command's nonzero status and file-level diagnostics.

**Acceptance**

- [ ] Allowed target-specific CLI and test dependencies pass. Aliased core-to-platform and sibling-to-sibling edges fail even for an inactive target.
- [ ] Existing core format, OS and unsafe checks still reject all their negative fixtures. Compiling `ferros-utils` enforces `unsafe_code = "forbid"`.
- [ ] Wrong-platform parser imports and signatures fail the fixtures. Simple statements that an interface can accept ELF or PE build IDs remain legal.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Extend `platform_boundary.rs` with synthetic manifests/graphs for Linux and Windows, an alias, an optional dependency and the legal factory edge. Exercise the public check command on the real workspace.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 02 — Commit birth-trap metadata only after a successful write

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed). Coordinate nearby task.rs edits with [#198](https://github.com/JamalLyons/ferros/issues/198) only if necessary.

**Why.** A write that covers an armed birth site changes its saved instruction and client-breakpoint flag before `write_target_memory` returns. An error leaves the earlier mutation visible through `mask_read` or a later disarm. The ordinary Session trap overlay already stages its changes until the backend succeeds. The existing injected failure fires before the faulty ordering. [src/birth.rs:206](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/birth.rs#L206) · [src/task.rs:2214](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L2214) · [src/birth.rs:192](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/birth.rs#L192) · [src/birth.rs:468](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/birth.rs#L468) · [session/breakpoints.rs:408](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/breakpoints.rs#L408) · [tests/code_writes.rs:150](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/code_writes.rs#L150)

**Scope.** `BirthPolicy`, `BirthSite`, `MacOSBackend::write_memory` and `write_target_memory` in `ferros-macos/src/{birth,task}.rs`. Existing tests in `ferros-tests/tests/code_writes.rs`.

**Design.** Prepare the bytes to write and the exact per-site updates without side effects. Apply those updates under the stopped target's serialization after the fallible memory write succeeds. Preserve internal traps on partial overlaps and cache invalidation on both outcomes.

```rust
fn plan_engine_write(&self, at: Address, bytes: &[u8]) -> Option<BirthWrite>;
fn commit_engine_write(&mut self, write: BirthWrite);
```

**Migration.** Start with a failed physical write following preparation. Move mutation into the commit operation. Audit detach/disarm and the shared-state lock so a pending exception cannot invalidate a prepared update.

**Acceptance**

- [ ] A refused write leaves the logical bytes, saved originals and ownership of all covered birth sites unchanged.
- [ ] A successful partial write updates only its covered bytes. A client trap remains observable and disarm restores the last successful original.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Extend the macOS code-write suite using an armed birth entry and a deterministic failure at the physical-write boundary after planning. Check readback, a subsequent successful retry, then disarm. Keep the existing cross-region rollback and birth masking tests.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 03 — Unwind an executable without source DWARF

**Size** L · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** Mach-O loads the executable mapping, then requires debug discovery before returning executable unwind tables. A missing companion keeps the image out of `SymbolCache`. With no image the unwinder can only try its frame-pointer path. `finish` uses that same caller. The existing no-DWARF test requests one frame, while the CFI tests enter below Session. [macho/image.rs:396](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/image.rs#L396) · [macho/image.rs:416](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/image.rs#L416) · [session/images.rs:356](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/images.rs#L356) · [symbols/unwind.rs:379](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/symbols/unwind.rs#L379) · [session/line_stepping.rs:144](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/line_stepping.rs#L144) · [tests/debuginfo.rs:291](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/debuginfo.rs#L291) · [tests/unwind_eh_frame.rs:210](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/unwind_eh_frame.rs#L210)

**Scope.** `objfile::{ImageContents,ObjectFormat}`, `symbols::{BinaryImage,SymbolCache}`, `session::{images,stack}`, `MockObjectFormat`, `ferros-macos/src/macho/{image,debuginfo,mod}.rs`.

**Design.** Keep validated executable metadata independently of optional source DWARF. File identity and slide constrain both. Executable CFI/hints remain usable on a source lookup failure. Keep companion-provided `.debug_frame` with source metadata and executable `.eh_frame` with the executable. Preserve the typed error for direct source-symbol requests and the public `SymbolCache::load_image` compatibility path.

```rust
struct ExecutableMetadata { mapping: ImageMapping, /* architecture, CFI, hints */ }
fn open_executable(&self, image: &ImageRequest) -> Result<ExecutableMetadata>;
fn source_debug(&self, image: &ImageRequest) -> Result<DebugSections>;
```

**Migration.** First add a Session test whose caller cannot be found by FP or LR. Split the adapter's discovery step, cache the two outcomes independently, then route stack walking and symbolication to the appropriate outcome. Keep section buffers shared.

**Acceptance**

- [ ] A mapped file with usable executable CFI and no companion yields the expected caller through `Session::stack_trace`; `Session::symbolicate` still reports `NoDebugInfo`.
- [ ] Existing hinted, inline, frame-pointer and external-debug behavior survives. No placeholder DWARF is necessary for unwind-only input.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add portable Session coverage to `unwind_eh_frame.rs` with FP zero and a misleading LR. Add macOS coverage in `debuginfo.rs` using a stripped/copy fixture with known executable unwind data. Check a caller and its return PC, plus `finish` where a caller is recoverable.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 04 — Revalidate disk metadata when a mapped build ID becomes known

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed), child 03 for the two metadata caches

**Why.** An unknown UUID allows a disk slice to be accepted without identity verification. A later known loader ID updates the Session image and selector, but keeps the old symbol handle. The next load returns early. If the open file belonged to another build, it can continue supplying names, lines and CFI. [macho/image.rs:276](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/image.rs#L276) · [macho/image.rs:314](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/image.rs#L314) · [session/images.rs:261](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/images.rs#L261) · [session/images.rs:340](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/images.rs#L340) · [tests/backend_mock.rs:2998](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/backend_mock.rs#L2998) · [tests/debuginfo.rs:464](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/debuginfo.rs#L464)

**Scope.** `SessionImage`, `SymbolCache`, `ImageContents`/executable metadata, `MachOFormat` and `MockObjectFormat`. `session/images.rs`, `symbols/{cache,image}.rs`, `objfile/{contents,mock}.rs`, `macho/image.rs`.

**Design.** Attach the build identity observed in the opened file to disk-derived metadata. On new positive evidence, retain verified matches. Evict contrary or unverified entries and reload through the validated selector. Keep still-valid target mappings, and keep mapped-memory symbol tables separate from disk data.

```rust
struct OpenedBuild { observed: BuildIdentity, slice: Option<ImageSlice> }
fn reconcile_disk_build(&mut self, mapped: BuildIdentity);
```

**Migration.** Drive unknown-to-known through a successful unverified open. Record the file's observed identity in the adapter. Reconcile before the cached-success shortcut and prune by the actual cache handle.

**Acceptance**

- [ ] Wrong-build DWARF and unwind bytes are inaccessible after a contradictory known ID arrives; matching files can still serve queries.
- [ ] Mapping retention on unknown-to-known, known-to-unknown tolerance, and `Absent` reuse only within one stop retain their existing coverage. A failed/missing slice cannot bypass verification.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Extend `backend_mock.rs` with counted opens for matching and differing observed IDs. Add a `debuginfo.rs` case with a mismatched thin file followed by a known mapped ID. Exercise both stack and symbol lookup after refresh.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 05 — Carry each physical frame's lookup address through symbol fallback

**Size** S · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** The unwind cursor distinguishes a return address from the exact interrupted PC. Session discards that distinction and subtracts four from every unnamed non-top frame. A signal interrupt at a function entry can therefore be displayed as the preceding function. Lazy image loading also probes PC minus four for every frame. [symbols/unwind.rs:349](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/symbols/unwind.rs#L349) · [symbols/unwind.rs:364](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/symbols/unwind.rs#L364) · [session/stack.rs:135](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/stack.rs#L135) · [session/stack.rs:159](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/stack.rs#L159) · [tests/backend_mock.rs:3165](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/backend_mock.rs#L3165)

**Scope.** `StackUnwinder`, `append_logical_frames`, `Session::stack_trace` and `name_frames_from_symbol_tables` in `symbols/unwind.rs` and `session/stack.rs`. Keep public `StackFrame` and `FrameId` semantics.

**Design.** Compute the lookup address once at the cursor. Propagate it with each logical frame for lazy image loading, DWARF and symbol-table naming. Keep raw PC, frame index and return address available to callers.

```rust
struct LocatedFrame { frame: StackFrame, lookup: Address }
fn unwind_located(&self, thread: ThreadId, regs: &Registers, limit: usize) -> Result<Vec<LocatedFrame>>;
```

**Migration.** First extend the existing synthetic signal-stack test with two adjacent named functions. Add the private located-frame result, then have the existing public unwind API project its frames.

**Acceptance**

- [ ] A non-top interrupted frame on a symbol boundary names the interrupted instruction's function.
- [ ] A regular caller at an image boundary still resolves at its call site. Inline expansion and the public raw PC remain stable.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Extend `backend_mock.rs` using `MockRuntime` plus mapped symbols with distinct entries at PC and PC minus four. Retain the regular-caller image-boundary and real signal backtrace tests.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 06 — Keep held signals until a resume actually starts their thread

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** `resume_with_signal` takes the carried list before the stopped-state check and before draining can fail. `resume()` also clears the list before validation. `resume_request` restores signals on one deferred-event path. An ordinary failure before backend resume or an invalid call while a stop is deferred can consume previously held intent. The handler path already removes signals only after a successful start. [session/lifecycle.rs:165](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/lifecycle.rs#L165) · [session/lifecycle.rs:198](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/lifecycle.rs#L198) · [session/lifecycle.rs:208](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/lifecycle.rs#L208) · [session/stepping.rs:208](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/stepping.rs#L208) · [session/stepping.rs:232](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/stepping.rs#L232) · [backend/types.rs:314](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/backend/types.rs#L314) Source stepping also drops the selected thread's signal before even reading registers. Instruction stepping already waits for a successful start. [session/line_stepping.rs:130](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/line_stepping.rs#L130) · [session/stepping.rs:498](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/stepping.rs#L498)

**Scope.** `Session::{resume,resume_with_signal,resume_request}`, `carried_signals`, and the existing start/refusal helpers in `session/{lifecycle,stepping,line_stepping}.rs`. Keep `StopEvent` delivery and the backend state machine.

**Design.** Validate state first. Prepare the requested deliveries without consuming held state. Commit only for threads actually allowed to run; apply explicit suppression at its matching valid transition. Distinguish successful starts, queued stops, delayed terminal events and failures. Reuse the existing refusal and stale-event filtering logic.

```rust
fn prepare_signals(&self, policy: SignalPolicy) -> PendingSignals;
fn commit_started(&mut self, started: &ResumeRequest);
```

**Migration.** First reproduce two distinct held signals plus a failed pre-start drain. Move signal ownership into one private helper and migrate plain, forwarding and handler continues plus source-step suppression. Check partial step-over success separately to prevent duplicate delivery.

**Acceptance**

- [ ] An invalid second resume and an injected pre-start failure preserve every still-held signal for the next valid forwarding resume.
- [ ] A successfully started thread receives its signal exactly once. Suspended peers retain theirs. Explicit suppression, zero-timeout wait and terminal cleanup continue to work.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Use `MockBackend::script_simultaneous` with different thread IDs and `fail_next(Wait, ...)` in `backend_mock.rs`. Assert exact per-thread signals on the eventual successful `MockCall::Resume`, plus retry after an invalid resume during a deferred stop. Inject a source-step register-read failure and verify the next forwarding continue still delivers the signal. Keep native signal-tracing regressions.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 07 — Distinguish synchronous faults from deliverable signals

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed). Child 06 is independent and owns the lifetime of queued intent.

**Why.** An access violation and a held soft signal can both surface as `StopEvent::Signal`. Session consequently builds a signal-delivery request for either. macOS retains forwarding eligibility only for soft signals and rejects the fault request. The mismatch follows directly from the decoder, held-stop bookkeeping and validator. [src/exception.rs:998](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/exception.rs#L998) · [src/exception.rs:1518](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/exception.rs#L1518) · [src/exception.rs:2093](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/exception.rs#L2093) · [src/task.rs:1643](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L1643) · [session/lifecycle.rs:198](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/lifecycle.rs#L198) · [src/exception.rs:2586](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/exception.rs#L2586)

**Scope.** `StopEvent` in `ferros-core/src/backend/types.rs`, `Session::resume_with_signal`, `ferros-macos/src/exception.rs` decoder and tests, `ferros/src/repl/{mod,output}.rs`, exhaustive matches and affected fixture assertions. Preserve the backend's existing validation.

**Design.** Introduce a typed fault stop carrying its thread and portable category. Translate native reasons in the adapter, keep raw detail for diagnostics, and reserve `Signal` for stops with a pending signal decision. After a fault, `resume_with_signal` uses ordinary continue semantics for that thread. Retain pending signal decisions for other threads. A persistent fault may stop again at the same PC.

```rust
enum FaultKind { MemoryAccess, IllegalInstruction, Arithmetic, Other }
// Proposed StopEvent variant:
// Fault { thread: ThreadId, kind: FaultKind, detail: String }
impl StopEvent { fn deliverable_signal(&self) -> Option<(ThreadId, i32)>; }
```

**Migration.** Pin fault and soft-signal behavior in a single public Session test before changing the enum. Update exhaustive consumers, CLI display and mock scripts in the same change. Leave native exception reply timing with its current owner.

**Acceptance**

- [ ] A fault can be identified and formatted without consulting its diagnostic text. A forwarding continue after a fault emits no signal request for that fault.
- [ ] Genuine soft signals still support suppression and forwarding, including a separately held signal on a peer. A repeated synchronous fault remains observable.
- [ ] Native reason translation stays in the platform crate. Session and CLI handle the new variant exhaustively.

**Tests to add.** Exercise a synchronous fault in a bounded macOS fixture. Inspect its event, resume once, accept the same fault if the instruction is retried, then tear down. Separately deliver an actual soft signal and verify its handler runs exactly once. Use the mock to assert the exact resume request for a fault with a held peer signal. Extend `crates/ferros-tests/tests/{exceptions,backend_mock}.rs` and the macOS decoder tests.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`, plus `just test-integration` on signed macOS arm64 and a check for leaked fixtures.

## Child 08 — Let the target supply its arm64 addressing and unwind conventions

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** A platform can construct `Arm64::with_addressable_bits` but cannot get that instance into Session's normal breakpoint or stack paths. One trap normalization call even bypasses `Session::arch`. The undefined-return-column CFI fallback also assumes the Apple CIE convention. An arm64 port would inherit both decisions before adding any CPU instructions. [arch/arm64.rs:15](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/arch/arm64.rs#L15) · [arch/mod.rs:195](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/arch/mod.rs#L195) · [session/breakpoints.rs:117](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/breakpoints.rs#L117) · [session/lifecycle.rs:404](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/lifecycle.rs#L404) · [symbols/unwind.rs:625](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/symbols/unwind.rs#L625) · [src/lib.rs:236](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/lib.rs#L236)

**Scope.** `Arch`, `Arm64`, `Session::arch`, stop normalization, `StackUnwinder::recover`, `MacOSBackend`, `MockBackend` and platform assembly. Keep target-specific choices in platform code.

**Design.** Select target facts from the attached/launched process via the backend or factory. Use an owned/shared Arch for the duration of an operation. Configure pointer width and the CFI undefined-return convention explicitly in the macOS adapter. Use one resolver for all callers. Preserve unsupported-architecture errors and host-before-launch semantics.

```rust
fn target_arch(&self) -> BackendResult<Arc<dyn Arch>>;
fn undefined_return_address(&self) -> UndefinedReturnAddress;
```

**Migration.** Secure the present raw/normalized PC behavior with an injected architecture test. Route both lookups through it. Test a second address width and strict undefined-return policy with neutral input before deleting the implicit platform choices.

**Acceptance**

- [ ] A Session backtrace can retain a valid address bit above 47 on a configured mock target, and macOS preserves its current address normalization.
- [ ] An explicitly undefined CFI return rule can end a trace on a strict target. The configured macOS case still unwinds its saved/live LR correctly.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add portable Session backtrace cases in `ferros-tests/tests/backend_mock.rs` or `unwind_eh_frame.rs`, covering both widths and CFI policies. Keep `for_architecture`'s unsupported x86-64 test and native prologue/epilogue cases.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 09 — Resolve the main executable explicitly in a symbol request

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** `Backend::images` promises a list of programs and libraries. Source breakpoint resolution silently uses element zero as the executable. It then enriches each site through `position_at`, which re-enters public symbolication and refreshes images again. Returning a correct but differently ordered list can resolve against the wrong module; one logical breakpoint with N sites can enumerate the loader N extra times. [backend/mod.rs:392](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/backend/mod.rs#L392) · [session/images.rs:221](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/images.rs#L221) · [session/location.rs:95](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/location.rs#L95) · [session/location.rs:173](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/location.rs#L173) · [session/stack.rs:299](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/stack.rs#L299) · [session/images.rs:250](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/images.rs#L250)

**Scope.** `Backend::images`, `LoadedImage` or a new `ImageList`, macOS image enumeration, `MockBackend`, `Session::{images,resolve_location,symbolicate}`, `session/{images,location,stack}.rs` and fixture constructors.

**Design.** Let the adapter identify the executable at enumeration. Internally bind resolution and position enrichment to one refreshed image list and request ID. Keep the existing public list order for macOS, cold/lazy symbol loading and explicit public refresh. Honor a missing main image as unresolved.

```rust
struct ImageList { main: Option<LoadedImage>, others: Vec<LoadedImage> }
fn position_in_request(&mut self, address: Address, prefer: Option<(&str, u32)>) -> SourcePosition;
```

**Migration.** Start with a mock listing a library before the executable and count `Images` calls for a multi-site query. Add an explicit role/list, migrate both adapters, then enrich sites using the existing request.

**Acceptance**

- [ ] File and function breakpoints resolve in the designated executable regardless of enumeration order. An absent main image does not choose a library.
- [ ] A multi-site resolution makes one loader enumeration, preserves source attribution for inline code and retains current ambiguity/pending behavior.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add a portable reordered-list test in `backend_mock.rs` and an enumeration-count assertion on a multi-site source breakpoint in `source_breakpoints.rs` using an instrumented adapter. Retain bootstrap main and attach cases.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 10 — Retry source discovery when its inputs change

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed), child 03

**Why.** A missing companion, a failed tool launch, a timeout and a cache-directory error all become `NoDebugInfo`. Session caches that diagnostic until unload and checks it before examining input stamps. A user who installs the correct companion after a lookup can remain stuck with the original failure for that loaded image. [macho/debuginfo.rs:169](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/debuginfo.rs#L169) · [macho/debuginfo.rs:188](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/debuginfo.rs#L188) · [macho/debuginfo.rs:224](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/debuginfo.rs#L224) · [macho/debuginfo.rs:253](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/macho/debuginfo.rs#L253) · [session/images.rs:334](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/session/images.rs#L334) · [tests/debuginfo.rs:337](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/tests/debuginfo.rs#L337)

**Scope.** `source_debug` result from child 03, `FailedLoad` and `SessionImage` in `session/images.rs`, `symbols/load_inputs.rs`, `macho/debuginfo.rs` and `objfile/mod.rs`. Limit cache changes to source data.

**Design.** Cache a diagnosis with its dependency stamp. Reuse current input tracking for a missing or mismatched companion; stamp watched directories as well as existing files. For transient tool failures with unchanged inputs, allow an explicit refresh and preserve the diagnostic until then. Reuse immutable executable metadata from child 03.

```rust
enum RetrySource { OnInputChange, OnExplicitRefresh }
fn refresh_debug_info(&mut self, image: &LoadedImage) -> Result<()>;
```

**Migration.** First add an adjacent companion between two Session lookups. Classify discovery outcomes inside the adapter; implement invalidation in core without naming files specific to a format. Cache the post-attempt stamp so a tool's temporary files cannot trigger a re-run loop.

**Acceptance**

- [ ] Adding a matching companion repairs a same-session symbol lookup without a relaunch. Stable missing inputs never run a tool per frame.
- [ ] A timed-out tool can be retried explicitly with the original error reported in the meantime. Executable unwind and mapped-layout caches remain valid.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Extend `debuginfo.rs` with late companion publication, tool timeout followed by explicit retry and an invocation counter for unchanged inputs. Retain existing corrupt-file, universal-slice and missing-debug diagnostics.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 11 — Preserve native memory error categories at the Backend seam

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** The contract distinguishes an unmapped address from denied access. A failed VM read is formatted as `io::Error::other`, then converted back to `BackendError`. A short successful read takes a different path and yields `NotFound`. API callers can receive different categories for the same unmapped range. This is a concrete #196 consumer, with a testable boundary. The short-page fallback also continues after adding the returned byte count; a zero-byte success would make no progress. [backend/mod.rs:339](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/backend/mod.rs#L339) · [src/memory.rs:601](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/memory.rs#L601) · [src/task.rs:163](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L163) · [src/task.rs:2193](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L2193) · [src/error.rs:352](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/error.rs#L352) · [src/error.rs:120](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/error.rs#L120) · [src/memory.rs:158](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/memory.rs#L158)

**Scope.** `ferros-macos/src/{memory,error,task}.rs`, VM read/cache paths and their `Backend` adapters. Keep the public structured `BackendError` and `DebuggerError` categories.

**Design.** Classify the native return where operation, address and code are known. Map unmapped/protection errors to the promised variants, preserve raw codes and context for other failures. Keep compatibility adapters for public native helpers. Scope this issue to memory reads and diagnostics originating there.

```rust
fn vm_read_error(operation: &'static str, at: Address, code: KernReturn) -> BackendError;
```

**Migration.** Pin direct Backend outcomes for unmapped and protected pages. Move conversion before text formatting, update cached and uncached reads, and verify Session and the CLI retain useful diagnostics.

**Acceptance**

- [ ] `Backend::read_memory` returns `NotFound` for unmapped and `PermissionDenied` for mapped unreadable bytes on both read paths.
- [ ] Unclassified VM errors keep the actual native code and address. Short reads remain all-or-nothing; no new OS types enter core.
- [ ] Zero progress on a short page returns an error promptly. Overflow or short data cannot produce a fabricated complete read.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add native tests to `code_writes.rs` or a dedicated memory-read file guarded by the macOS harness. Use a fixture with documented mapped protections; complement with table-driven adapter tests for deterministic return codes. Assert variants at the Backend boundary. Exercise a short page followed by a zero-byte read with a deterministic reader.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 12 — Classify terminal signals at the CLI platform boundary

**Size** S · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed)

**Why.** `continue` suppresses numbers 2 and 18 and prints job-control guidance for 21 and 22. The help text repeats 18. For example, a target using 20 for its terminal-stop signal takes the delivery branch. A new backend can return a valid raw signal and still get the wrong CLI policy. [repl/mod.rs:43](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/repl/mod.rs#L43) · [repl/mod.rs:157](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/repl/mod.rs#L157) · [repl/mod.rs:233](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/repl/mod.rs#L233) · [commands/process.rs:87](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/repl/commands/process.rs#L87) · [src/platform.rs:46](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/platform.rs#L46) · [backend/types.rs:39](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/backend/types.rs#L39)

**Scope.** `ferros/src/{platform,main}.rs`, `repl/{mod,jobs,tests}.rs`, `repl/commands/process.rs`, macOS signal constants/classifier in `ferros-macos/src/terminal.rs`.

**Design.** Inject a small target signal classifier independent of whether stdin is a terminal. The platform supplies native constants. The REPL selects suppress/pass and displays background-I/O guidance, retaining the raw value in stop output. Make help use signal names.

```rust
enum TerminalSignal { Interrupt, Suspend, BackgroundIo, Other }
fn classify_signal(&self, raw: i32) -> TerminalSignal;
```

**Migration.** Add a fake signal map and transcript expectations. Wire macOS through the existing platform factory. Exercise pipe and terminal modes to ensure `NoJobControl` does not erase classification.

**Acceptance**

- [ ] Suppression and background-I/O guidance follow the injected mapping; unrelated raw signals are passed through.
- [ ] macOS interactive, pipe and batch behavior stays consistent. Generic help contains no fixed platform signal number.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add REPL tests with two different mappings for 18 and 20, checking the actual `MockCall::Resume` signal, help text and hint. Keep existing PTY job-control coverage.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 13 — Separate harness supervision from platform cleanup

**Size** M · **Dependencies** Independent of other children; [#193](https://github.com/JamalLyons/ferros/issues/193) already supplies the platform Session factory.

**Why.** Taking `impl Backend` alone, as discussed in #196, would lose the side handle used to reap a detached child. The harness creates the backend, obtains `child_cleanup`, constructs the Session, tags processes, then invokes absolute `ps`, `kill` and `codesign` paths. These are different responsibilities. Porting the generic deadline runner verbatim would copy native cleanup decisions. [src/harness.rs:104](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L104) · [src/harness.rs:269](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L269) · [src/harness.rs:312](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L312) · [src/harness.rs:590](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L590) · [src/harness.rs:690](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L690) · [src/harness.rs:838](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L838) · [src/harness.rs:919](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L919) · [src/harness.rs:1017](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-tests/src/harness.rs#L1017)

**Scope.** `ferros-tests/src/{harness,lib}.rs`, a test-private macOS adapter and harness tests. Retain `launch_with_backend` as a native convenience for preconfigured fault hooks. Avoid changes to `Backend` for test supervision.

**Design.** Separate the launch pair from host-side supervision for finding a test-owned tag, killing, reaping and reporting a stalled launch. Keep live Sessions on the test worker. Expose the supervisor core to a deterministic test double off macOS. Only the real host adapter knows signing, PID discovery and native reap semantics.

```rust
struct TestLaunch { session: Session, cleanup: Box<dyn ChildCleanup> }
trait ChildCleanup { fn reap(&mut self) -> std::io::Result<()>; }
```

**Migration.** Use the current factory and cleanup handle as the first adapter. Move tag-scoped sweeps, native reaping and denial guidance behind that boundary. Exercise deadline/panic ordering against a recording supervisor before enabling any new OS.

**Acceptance**

- [ ] The same deadline/cleanup coordinator runs with a recording host on any OS; native tests still accept prepared macOS backends.
- [ ] Timeout, panic, detach and failed launch clean only the owning test's processes. Preserve kill-before-reap ordering, capture cleanup and the native permission guidance.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add a portable no-process supervisor test for cleanup after timeout with two distinct tags and a delayed launch. Keep `ferros-tests/tests/harness.rs` real-process tests for panic, timeout, detach and no leaks. Assert no unrelated tag is reaped, including when the two tests overlap. Keep signed native tests in `harness::run`.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 14 — Give the CLI ownership of its logging worker

**Size** S · **Dependencies** Independent

**Why.** The console-plus-file branch drops the nonblocking worker guard at initialization. The file-only branch forgets it, so normal shutdown cannot flush through Drop. Both use `.init()` despite a fallible public contract. The CLI tests the result and immediately discards a successful value; `find-logs` separately reconstructs the directory. [src/logging.rs:287](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-utils/src/logging.rs#L287) · [src/logging.rs:382](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-utils/src/logging.rs#L382) · [src/logging.rs:395](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-utils/src/logging.rs#L395) · [src/logging.rs:243](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-utils/src/logging.rs#L243) · [src/main.rs:105](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/main.rs#L105) · [src/main.rs:156](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros/src/main.rs#L156)

**Scope.** `ferros-utils/src/{logging,lib}.rs`, `ferros/src/main.rs` initialization and `find-logs`. Preserve public entry points used by the parked client.

**Design.** Return an owned flush guard to the live CLI and hold it through normal exit. Use a fallible appender and subscriber installation. For compatibility initializers with no owner, use a synchronous file writer or another explicitly managed lifetime. Resolve the directory once using OS paths and `temp_dir()` as the fallback.

```rust
struct LoggingSession { path: PathBuf, worker: WorkerGuard }
fn start_file_logging(level: Option<LogLevel>, format: LogFormat) -> Result<LoggingSession, LoggingError>;
```

**Migration.** Start with a subprocess that emits a final record and exits immediately. Add the owned API and move the CLI to it. Share path resolution and cover duplicate initialization without modifying the test runner's global subscriber.

**Acceptance**

- [ ] Final records survive an orderly CLI exit in pretty and JSON modes, and file-only logging keeps transcripts clean.
- [ ] Duplicate subscriber initialization and an unwritable destination return typed errors. `find-logs` reports the directory used by initialization, including a non-Unicode home where supported.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add isolated subprocess logging tests covering both formats, double installation and file failure. Compare `find-logs` with a session's actual log path. Keep parser tests.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.

## Child 15 — Make a timed-out exception continuation distinguishable from a completed one

**Size** M · **Dependencies** [#193](https://github.com/JamalLyons/ferros/issues/193) (closed). Integrate after the adjacent [#198](https://github.com/JamalLyons/ferros/issues/198) exception-loop changes.

**Why.** Native resume enqueues `Continue`, waits for an acknowledgement, then restores local bookkeeping on timeout. The loop consumes held replies, clears `pending_exception`, and ignores a failed acknowledgement send. The command carries no identity or cancellation state. A caller timeout is therefore ambiguous: the loop may still process the old command. This is an ordering risk inferred from the channel protocol; its native frequency is unknown. [src/task.rs:1981](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L1981) · [src/task.rs:2046](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L2046) · [src/task.rs:2058](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/task.rs#L2058) · [src/exception.rs:751](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/exception.rs#L751) · [src/exception.rs:1141](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-macos/src/exception.rs#L1141) · [backend/mod.rs:51](https://github.com/JamalLyons/ferros/blob/7751824b6c2f5e769900675a3f0e31913a2d2b69/crates/ferros-core/src/backend/mod.rs#L51)

**Scope.** `ExceptionLoopCommand::Continue`, native resume rollback and acknowledgements in `ferros-macos/src/{task,exception}.rs`. Preserve separate private detach/shutdown semantics. No change to the #198 suspend retry policy.

**Design.** Give each continuation a one-use shared ticket. The loop must claim the ticket before consuming replies. A timeout cancels a still-pending ticket; a claimed ticket must be reconciled before allowing another resume. A completed disposition remains observable after an acknowledgement receiver goes away. Do not issue an additional task resume for a late completion.

```rust
enum ContinueState { Pending, Claimed, Completed, Cancelled }
fn cancel_if_pending(&self) -> ContinueState;
```

**Migration.** Use a deterministic test command adapter to delay receipt versus delay of the acknowledgement. Verify current failure semantics first. Introduce claim/cancel at the command boundary, retain all-stop ownership during reconciliation and give an unrecoverable outcome an explicit error if required.

**Acceptance**

- [ ] Cancelled unclaimed commands cannot consume a later stop's replies or publish completion for a newer attempt.
- [ ] Completed commands with a lost acknowledgement are recognized once. Retry cannot redeliver a signal, resume the task twice or erase a later held stop. Preserve a bounded failure path on loop death.
- [ ] Regression exercises the affected interface and fails against the previous behavior.

**Tests to add.** Add macOS command-state tests with barriers on either side of the claim and a signed harness case that delays acknowledgement for one continue, retries and verifies the held thread and signal once. Do not reuse #198's suspend-retry tests as coverage for command acknowledgement.

**Checks.** `just fmt-check`, `just lint`, `just test`, `just check-boundary`; on signed macOS arm64 also `just test-integration` and confirm no worktree fixture remains.
