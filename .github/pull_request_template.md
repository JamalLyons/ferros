## What and why

<!-- Link the issue: Closes #n or Part of #n. -->

## Checks

- [ ] `just fmt-check`
- [ ] `just lint`
- [ ] `just test`
- [ ] `just test-integration` (then `pgrep -fl ferros-fixture-` prints nothing from this worktree)
- [ ] `just check-boundary`
- [ ] An engine change has an integration test in `crates/ferros-tests` that fails without it (or: no engine change)
