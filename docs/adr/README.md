# Architecture decision records

Records of design choices. An **Accepted** record is an owner decision. A **Proposed** record is not a rule. [`AGENTS.md`](../../AGENTS.md) is the source of truth for how to change the repository, and [`docs/architecture.md`](../architecture.md) describes the engine as it is.

| Record | Status | Tracking | Decision |
|---|---|---|---|
| [0001-object-file-boundary](0001-object-file-boundary.md) | Accepted (2026-11-22) | [#193](https://github.com/JamalLyons/ferros/issues/193) | Format-neutral image/object-file trait in `ferros-core`. Mach-O implementation only in `ferros-macos`. DWARF stays in core. |
