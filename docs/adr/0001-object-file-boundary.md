# 0001. An object-file boundary in ferros-core before a Linux backend

- Status: Accepted
- Date: 2026-11-22
- Accepted: owner decision 2026-11-22
- Tracking: [#193](https://github.com/JamalLyons/ferros/issues/193) (Phase 1 sub-issue of [#26](https://github.com/JamalLyons/ferros/issues/26))
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

Issue #41 schedules a Linux `ptrace` backend as a later epic and says it is the real test of the platform boundary. The list above is the part that has to move first, or the Linux crate will grow a second copy of symbolication. [#193](https://github.com/JamalLyons/ferros/issues/193) tracks that move and is a Phase 1 sub-issue of epic #26. This record does not move the code.

DWARF consumption (`gimli`, `addr2line`, line tables, function indexes) is already format-neutral once the sections and the segment slide are known. That code should stay in `ferros-core`. The leak is the container: magic numbers, load commands, UUID width, compact unwind, and the shared-cache symbol table.

## Options considered

### A. Leave container parsing in `ferros-core` and add ELF beside it

Keep `MH_MAGIC_64`, `LC_SYMTAB`, and compact unwind where they are. A Linux backend would fill `LoadedImage` as best it can, and new ELF parsers would land next to the Mach-O ones inside `Session` and `symbols`.

This is the smallest change and matches how the file parser already calls `object::File`. It also makes the "portable" crate the place that knows every container. `LoadedImage.uuid` still cannot hold a 20-byte build-id, and `same_build` would keep equating missing ids. A third format would add another pile of constants to the session.

### B. An image-format boundary inside `ferros-core`, implemented for Mach-O now

Add a small trait (or a focused module with one implementor) that `Session` calls for container questions: recognize a mapped header, parse the segment slide, locate the symbol table and unwind blobs, and return a build identity that can hold a 16-byte UUID or a longer build-id. The Mach-O implementation moves behind that trait, still inside `ferros-core`, with no `unsafe` and no OS API. `MacOSBackend::images` stays the dyld enumeration. DWARF parsing stays in `symbols` and consumes the trait's output. An ELF implementor is a later change, in this crate, not in a platform crate. Direct `object` dependencies move to one version so the engine and `ferros-macos` share header types; the 0.37 copy stays until `addr2line` no longer pulls it in.

`Session` remains free of `cfg(target_os)`. The format is chosen from the bytes (or from an explicit value on `LoadedImage`), not from the host OS. A macOS process can load a Mach-O; a Linux process can load an ELF; the host OS does not decide the parser.

Not chosen. The Mach-O implementor would still be macOS format knowledge inside `ferros-core`. The owner rule is that no platform's format code lives in core, and that one platform's format code does not live in another platform crate.

### C. Push all container parsing into the platform crate

`Backend` would return portable segments, symbol names, and unwind rows. `ferros-core` would never mention Mach-O or ELF. `MacOSBackend` would own `symtab.rs`, compact unwind, and UUID reads.

This hides format details from the engine, but it moves policy into the mechanism trait. Symbol names and unwind are how `Session` builds a backtrace; `MockBackend` would have to invent them too. Every platform crate would also take a DWARF dependency or would hand raw sections back, which is option B with the trait defined on `Backend` instead of beside it. `Backend` is documented as synchronous process control, sized against the GDB remote protocol, and hardware breakpoints are already kept out of it. Container parsing is a poor fit for that trait: it does not need a task port, and the tests that parse a synthetic Mach-O (`crates/ferros-tests/tests/unwind_eh_frame.rs`, `session` symtab tests) do not have a process.

### D. Format-neutral trait in `ferros-core`, Mach-O implementor in `ferros-macos`

`ferros-core` defines an image/object-file trait whose types and methods do not name a container: recognize a mapped header, report the segment slide, locate symbol-table and unwind bytes, and return a build identity wide enough for a 16-byte UUID or a longer build-id. `Session` calls that trait. It does not scan for magic numbers.

The Mach-O implementor lives in `ferros-macos`: `MH_MAGIC_64` scanning, `LC_SYMTAB` / `LC_FUNCTION_STARTS` / `__LINKEDIT`, Apple compact unwind (`__unwind_info`), and `LC_UUID`. DWARF consumption (`gimli`, `addr2line`, line tables, function indexes) stays in `ferros-core` and reads the section bytes the trait returns.

A future `ferros-linux` implementor knows ELF and does not know Mach-O. A future `ferros-windows` implementor knows PE/COFF and does not know Mach-O or ELF. Neither implementor is compiled into `ferros-core` or into the other platform crate. The trait is not added to `Backend`: process control and container parsing stay separate, and `MockBackend` keeps returning `LoadedImage` values. Tests that need a Mach-O parser use the macOS implementor or a neutral test double, not Mach-O constants in core.

## Decision

Accepted. Option D. Owner decision 2026-11-22.

`ferros-core` is fully platform-neutral and format-neutral. It holds a format-neutral image/object-file trait and the DWARF layer. All macOS code, including Mach-O object-file knowledge (`MH_MAGIC` scanning, `LC_SYMTAB`, compact unwind, `LC_UUID`), lives only in `ferros-macos`. Future `ferros-linux` and `ferros-windows` code never leaks into each other or into `ferros-core`.

Do not add container methods to `Backend`. Widen build identity so a 20-byte ELF build-id fits, and stop using "both ids missing" as proof of the same build unless the format implementor says a missing id is not evidence (the macOS case of a failed `LC_UUID` read has to stay distinct from "this format has no id"). The `object` crate's Mach-O feature leaves `ferros-core` with the parsers; direct dependencies collapse to one version as part of that move.

No code moved in the pull request that recorded this decision. [#193](https://github.com/JamalLyons/ferros/issues/193) did the move: the trait is `ferros_core::objfile::ObjectFormat`, the Mach-O implementor is `ferros_macos::macho::MachOFormat`, and `ferros-core` no longer depends on `object`. The Context section describes the code before that move. New Mach-O, ELF, or PE knowledge does not go into `ferros-core`, and one platform crate does not gain another's format code.

## Consequences

Once [#193](https://github.com/JamalLyons/ferros/issues/193) implements this decision:

- `ferros-macos` implements the image/object-file trait for Mach-O and implements `Backend` for Mach process control. `Session` still owns breakpoints, stepping, and DWARF.
- A Linux crate implements `Backend` and the ELF side of the same trait. It does not reimplement breakpoints, stepping, or DWARF, and it does not contain Mach-O parsers. Windows is the same shape for PE/COFF.
- `LoadedImage.uuid: Option<[u8; 16]>` changes shape. Call sites in `ferros-core`, `ferros-macos`, `ferros`, and `ferros-tests` construct that array today. The neutral identity is not an `LC_UUID`.
- `same_build` stops treating two missing ids as the same image. The #170 behavior (a UUID that appears on a later stop) stays a Mach-O implementor rule, not a core rule that every missing id is equal.
- The page scan for unlisted shared-cache images moves to `ferros-macos`. `session/stack.rs` no longer contains `MH_MAGIC_64`. ELF has no dyld shared cache and does not scan for that magic.
- Compact unwind moves to `ferros-macos`. Instruction decoding that is really arm64 (`MODE_FRAMELESS` stack size, frame pointer) can remain with `Arch` only where it does not name a Mach-O section. The choice of `__unwind_info` versus `.eh_frame` belongs to the format implementor.
- `MockBackend` keeps returning `LoadedImage` values. A unit test that must parse Mach-O bytes depends on the macOS implementor or on a neutral double. It does not embed `MH_MAGIC_64` in `ferros-core`.
- `just check-boundary` today rejects OS crates, `cfg(target_os)`, and `unsafe` in `ferros-core`. It does not yet reject Mach-O. Extending it is part of #193, not of this record.
