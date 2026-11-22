# 0001. An object-file boundary in ferros-core before a Linux backend

- Status: Proposed
- Date: 2026-11-22
- Related: [Epic #26](https://github.com/JamalLyons/ferros/issues/26) (Phase 1, especially deferred criterion C16), [issue #41](https://github.com/JamalLyons/ferros/issues/41) (a Linux backend is the later test of the platform boundary)

## Context

Epic #26 asked for one backend trait so a Linux or Windows crate could be added later without refactoring the engine. That split is in place for process control. `Backend` (`crates/ferros-core/src/backend/mod.rs`) is mechanism only: launch, attach, registers, memory, threads, regions, images, and `StopEvent`. `Session` (`crates/ferros-core/src/session/`) owns breakpoints, stepping, and symbolication. `ferros-core` has no `mach2`, `libproc`, `libc`, `cfg(target_os)`, or `unsafe`. `MacOSBackend` (`crates/ferros-macos/src/task.rs`) and `MockBackend` are the two implementors. `just check-boundary` enforces that OS boundary.

The object-file format did not get the same boundary. The engine still assumes a 64-bit Mach-O image in the code that a Linux backend would have to share:

- Unlisted images are found by scanning mapped pages for `MH_MAGIC_64` (`0xfeedfacf`) and parsing the hit with `SymtabLayout::from_header`. That scan lives in `Session` (`crates/ferros-core/src/session/stack.rs`), not in the macOS crate. The same magic is also checked in `crates/ferros-macos/src/birth.rs` and `crates/ferros-macos/src/images.rs`, which is where a platform constant belongs.
- Names for code without DWARF come from a Mach-O symbol table. `crates/ferros-core/src/symbols/symtab.rs` parses `LC_SYMTAB`, `LC_FUNCTION_STARTS`, `nlist64`, and `__LINKEDIT`, including the dyld shared-cache layout. `Session::symbol_table_name` reads that layout from the mapped header (`session/stack.rs`).
- Stack unwinding prefers Apple compact unwind. `crates/ferros-core/src/symbols/compact_unwind.rs` looks up `__TEXT,__unwind_info`, and `crates/ferros-core/src/symbols/unwind.rs` decodes `UNWIND_ARM64_MODE_*` before falling back to `__eh_frame`. An ELF image's unwind info is `.eh_frame` / `.debug_frame`, not this section.
- Build identity is a Mach-O UUID. `LoadedImage.uuid` is `Option<[u8; 16]>` and is documented as `LC_UUID` (`crates/ferros-core/src/backend/types.rs`). A GNU build-id is often 20 bytes (SHA-1) and can be another length. Those bytes do not fit in this field, so an ELF backend cannot report the id the engine would need to tell two builds apart.
- `same_build` (`crates/ferros-core/src/session/images.rs`) treats any missing id as "not a different build": only two present, different UUIDs fail the check. That matches a macOS UUID read that can fail at one stop and succeed at the next (issue #170). It also treats two images with no id as the same image. For ELF, a missing build-id is common, and a rebuilt file at the same path and load address would keep the previous DWARF.
- Container parsing is Mach-O even where DWARF itself is not. `crates/ferros-core/src/symbols/image.rs` reads mapped headers as `MachHeader64`, skips `__PAGEZERO`, and takes `mach_uuid`. `crates/ferros-core/src/symbols/debuginfo.rs` picks a slice out of a Mach-O universal file (`MachOFatFile32` / `MachOFatFile64`) by CPU subtype and `LC_UUID`. The `object` crate can parse ELF and PE, and `BinaryImage` asks it for DWARF sections by both ELF and Mach-O names, but no session path loads an ELF image.
- Three copies of the `object` crate are in `Cargo.lock`: 0.37.3 pulled in by `addr2line` 0.25.1, 0.38.0 as a direct dependency of `ferros-core` and `ferros-tests`, and 0.41.0 as a direct dependency of `ferros-macos`. The workspace allows `clippy::multiple_crate_versions`. Header types from 0.38 and 0.41 are not the same type, so the backend and the engine cannot hand each other a parsed Mach-O header without serializing bytes first.

The open sub-issues of epic #26 are the owner walkthrough (#40) and three bug fixes (#183, #190, #191). None of them is this boundary. Issue #41 schedules a Linux `ptrace` backend as a later epic and says it is the real test of the platform boundary. `docs/architecture.md` tells that future crate that nothing in `ferros-core` has to change. The list above is the part that has to change first, or the Linux crate will grow a second copy of symbolication.

DWARF consumption (`gimli`, `addr2line`, line tables, function indexes) is already format-neutral once the sections and the segment slide are known. That code should stay in `ferros-core`. The leak is the container: magic numbers, load commands, UUID width, compact unwind, and the shared-cache symbol table.

## Options considered

### A. Leave container parsing in `ferros-core` and add ELF beside it

Keep `MH_MAGIC_64`, `LC_SYMTAB`, and compact unwind where they are. A Linux backend would fill `LoadedImage` as best it can, and new ELF parsers would land next to the Mach-O ones inside `Session` and `symbols`.

This is the smallest change and matches how the file parser already calls `object::File`. It also makes the "portable" crate the place that knows every container. `LoadedImage.uuid` still cannot hold a 20-byte build-id, and `same_build` would keep equating missing ids. A third format would add another pile of constants to the session.

### B. An image-format boundary inside `ferros-core`, implemented for Mach-O now

Add a small trait (or a focused module with one implementor) that `Session` calls for container questions: recognize a mapped header, parse the segment slide, locate the symbol table and unwind blobs, and return a build identity that can hold a 16-byte UUID or a longer build-id. The Mach-O implementation moves behind that trait, still inside `ferros-core`, with no `unsafe` and no OS API. `MacOSBackend::images` stays the dyld enumeration. DWARF parsing stays in `symbols` and consumes the trait's output. An ELF implementor is a later change, in this crate, not in a platform crate. Direct `object` dependencies move to one version so the engine and `ferros-macos` share header types; the 0.37 copy stays until `addr2line` no longer pulls it in.

`Session` remains free of `cfg(target_os)`. The format is chosen from the bytes (or from an explicit value on `LoadedImage`), not from the host OS. A macOS process can load a Mach-O; a Linux process can load an ELF; the host OS does not decide the parser.

### C. Push all container parsing into the platform crate

`Backend` would return portable segments, symbol names, and unwind rows. `ferros-core` would never mention Mach-O or ELF. `MacOSBackend` would own `symtab.rs`, compact unwind, and UUID reads.

This hides format details from the engine, but it moves policy into the mechanism trait. Symbol names and unwind are how `Session` builds a backtrace; `MockBackend` would have to invent them too. Every platform crate would also take a DWARF dependency or would hand raw sections back, which is option B with the trait defined on `Backend` instead of beside it. `Backend` is documented as synchronous process control, sized against the GDB remote protocol, and hardware breakpoints are already kept out of it. Container parsing is a poor fit for that trait: it does not need a task port, and the tests that parse a synthetic Mach-O (`crates/ferros-tests/tests/unwind_eh_frame.rs`, `session` symtab tests) do not have a process.

## Decision

Option B, proposed and not yet implemented.

Before a Linux backend, put an image-format boundary under `ferros-core` and move the Mach-O assumptions listed in Context behind it. Keep DWARF in `ferros-core`. Do not add container methods to `Backend`. Widen build identity so a 20-byte ELF build-id fits, and stop using "both ids missing" as proof of the same build unless the format says a missing id is not evidence (the macOS case of a failed `LC_UUID` read has to stay distinct from "this format has no id"). Collapse the direct `object` 0.38 and 0.41 dependencies to one version as part of that move.

No code moves in this record. A later change adopts it by landing the trait with the Mach-O implementor and an integration test that fails if `Session` scans for `MH_MAGIC_64` itself.

## Consequences

- A Linux crate implements `Backend` and, separately, the image-format trait (or selects the ELF implementor). It does not reimplement breakpoints, stepping, or DWARF.
- `LoadedImage.uuid: Option<[u8; 16]>` changes shape. Call sites in `ferros-core`, `ferros-macos`, `ferros`, and `ferros-tests` construct that array today.
- `same_build` gains a third outcome or a richer id, so a missing id is not silently "same". The #170 behavior (a UUID that appears on a later stop) has to keep working for Mach-O.
- The page scan for unlisted shared-cache images becomes a Mach-O strategy the session invokes, not a constant in `session/stack.rs`. ELF has no dyld shared cache and should not scan for `MH_MAGIC_64`.
- Compact unwind stays the Mach-O source of unwind records. Instruction decoding that is really arm64 (`MODE_FRAMELESS` stack size, frame pointer) can remain with `Arch`. The choice of section does not.
- `MockBackend` keeps returning `LoadedImage` values. Tests that parse Mach-O bytes keep compiling on Linux, because the Mach-O implementor has no OS dependency.
- Until this is adopted, `docs/architecture.md` is wrong if it says a new platform crate needs no change in `ferros-core`. The process-control trait can stay as it is; the image format cannot.
