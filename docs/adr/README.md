# Architecture decision records

Records of design choices that are still open, or that later changes adopt. A record with status **Proposed** is not a rule. [`AGENTS.md`](../../AGENTS.md) is the source of truth for how to change the repository, and [`docs/architecture.md`](../architecture.md) describes the engine as it is.

| Record | Status | Decision |
|---|---|---|
| [0001-object-file-boundary](0001-object-file-boundary.md) | Proposed | Put Mach-O container parsing behind an image-format boundary in `ferros-core` before adding a Linux backend. |
