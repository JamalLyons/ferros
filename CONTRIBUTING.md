# 🤝 Contributing to Ferros

Welcome to **Ferros**, the Rust-native debugger built from the same element as Rust itself.
We’re excited that you’re interested in contributing! This document outlines how to set up your environment, submit contributions, and collaborate with the team.

---

## 🧭 Project Overview

**Ferros** is an open-source debugger built for Rust developers.
Its goal is to provide a first-class debugging experience that understands Rust’s unique semantics — ownership, lifetimes, and MIR.

Our architecture includes:
- `crates/ferros`: Command-line interface.
- `crates/ferros-core`: Portable debugging engine (Session, policy, arch, symbols, breakpoints; zero OS-specific code).
- `crates/ferros-macos`: macOS implementation (Mach APIs, posix_spawn, exception handling, task ports).
- `crates/ferros-tests`: Integration test suite, test harness, and fixture binaries.
- `crates/ferros-utils`: Shared utilities, structured logging (`tracing`).
- `crates/ferros-mir`: MIR-level interpretation and analysis (reserved for future compiler integration).
- `crates/ferros-protocol`: Communication protocol and remote debugging (reserved for future protocol support).
- `crates/ferros-ui`: Terminal User Interface (currently parked and excluded from workspace while being rebuilt).

---

## 🧰 Prerequisites

Before contributing, make sure you have the following installed:

- 🦀 [Rust toolchain](https://www.rust-lang.org/tools/install): Rust 1.100.0 with `clippy` (pinned automatically by [`rust-toolchain.toml`](./rust-toolchain.toml)), plus a nightly toolchain with `rustfmt` for the unstable options in `rustfmt.toml` (`just install-nightly`)
- 🧱 `cargo` and `rustup`
- 🧪 [Git](https://git-scm.com/)
- ⚡ [`just`](https://github.com/casey/just) task runner (`brew install just` or `cargo install just`)
- 🍏 macOS (Apple Silicon arm64) for debugging real targets; Linux and Windows are planned

To verify your setup:
```bash
rustc --version
cargo --version
just --version
```

---

## 🏗️ Getting Started

1. **Fork and clone the repository**
   ```bash
   git clone https://github.com/JamalLyons/ferros.git
   cd ferros
   ```

2. **Run setup**
   ```bash
   just setup
   ```
   Or manually build the workspace:
   ```bash
   cargo build --workspace
   ```

3. **Create a new branch**
   ```bash
   git checkout -b feature/my-feature
   ```

4. **Run the tests**
   ```bash
   just test              # Unit tests
   just test-integration  # Integration tests (runs signed fixtures)
   ```

---

## 📦 Project Structure

```
ferros/
 ├─ crates/
 │   ├─ ferros/           # Command-line interface
 │   ├─ ferros-core/      # Portable debugging engine (Session, policy, arch, symbols; no OS code)
 │   ├─ ferros-macos/     # macOS implementation (Mach APIs, exception ports, task_for_pid)
 │   ├─ ferros-mir/       # MIR integration & analysis (reserved for future compiler integration)
 │   ├─ ferros-protocol/  # Communication & remote debugging protocol (reserved)
 │   ├─ ferros-tests/     # Integration test suite, harness, and fixture programs
 │   ├─ ferros-ui/        # Terminal User Interface (parked while being rebuilt)
 │   └─ ferros-utils/     # Shared logging (tracing) and utility helpers
 ├─ docs/                 # Architecture and design documentation
 ├─ scripts/              # CI test runner and platform boundary checker
 ├─ website/              # Ferros landing page and simulation
 ├─ Cargo.toml            # Workspace manifest
 ├─ JustFile              # Development task runner recipes
 ├─ rust-toolchain.toml   # Pinned Rust 1.100.0 toolchain
 ├─ AGENTS.md             # Contributor and agent guidance
 └─ ROADMAP.md            # Project goals and milestones
```

---

## 🧩 Types of Contributions

You can contribute in many ways:

- 🐞 **Report bugs:** Use the “Bug report” issue template.
- 🧠 **Propose features:** Open a “Feature request” issue or RFC discussion.
- 💻 **Submit code:** Implement tasks from the roadmap or fix open issues.
- 🧾 **Improve docs:** Clarify, expand, or add new documentation.
- 🧩 **Refactor:** Improve code structure, readability, or performance.

Check [ROADMAP.md](./ROADMAP.md) for ideas on what to work on.

---

## ✏️ Code Style & Standards

Ferros follows **Rust’s standard style guidelines** enforced by `rustfmt` and `clippy`.

Before committing:
```bash
just fmt-check
just lint
just check-boundary
```

### Safety & Platform Boundary Guidelines
- **Zero Unsafe in Core**: `unsafe_code = "forbid"` is set across the workspace and `crates/ferros-core` declares `#![forbid(unsafe_code)]`.
- **Platform Boundary**: `crates/ferros-core` contains no operating-system dependencies (`mach2`, `libproc`, `libc`, platform crates) and no `cfg(target_os)`. OS code belongs exclusively in platform crates (`crates/ferros-macos`).
- **Factory Isolation**: The CLI names platform crates only in `crates/ferros/src/platform.rs`.
- **Documented APIs**: Document public functions with Rustdoc comments (`///`).
- Keep PRs small and focused.

### Verification Checklist Before Submitting a PR
Ensure all five verification checks pass:
1. `just fmt-check` — Code formatting matches `rustfmt.toml`
2. `just lint` — Clippy passes with warnings denied
3. `just test` — Workspace unit tests and doctests pass
4. `just test-integration` — Real process integration tests pass
5. `just check-boundary` — Platform boundary integrity is maintained

---

## 🧪 Testing

All new code should include tests when applicable. Every engine change should land with a test that exercises it.

- **Run unit tests and doctests** (requires no special permissions):
  ```bash
  just test
  # or: cargo test --workspace --exclude ferros-tests --all-features
  ```

- **Run integration tests** (launches real fixture processes):
  ```bash
  just test-integration
  # or: cargo test -p ferros-tests
  ```
  On arm64 macOS, `scripts/cargo-test-runner.sh` automatically signs the test executable with `crates/ferros-macos/ferros.entitlements` (`com.apple.security.cs.debugger`). No `sudo` is required.

- **Run tests for an individual crate**:
  ```bash
  just test-crate ferros-core
  ```

- **Integration test fixtures**: Fixture source files are in `crates/ferros-tests/fixtures/` and integration tests are in `crates/ferros-tests/tests/`.

---

## 🗃️ Commit & PR Guidelines

- Write **clear, descriptive commit messages**:
  ```
  feat(core): step over software breakpoints and hide traps in memory
  feat(macos): single-step threads and keep others suspended on resume
  fix(core): clear process ownership on exit
  docs: describe platform boundary and update architecture guide
  ```

- Follow the [Conventional Commits](https://www.conventionalcommits.org/) style:
  - `feat:` – new feature
  - `fix:` – bug fix
  - `refactor:` – non-breaking code improvements
  - `docs:` – documentation updates
  - `test:` – adding or improving tests
  - `chore:` – tooling or maintenance

- Each PR should:
  - Reference related issues (`Closes #42`).
  - Pass CI and all 5 verification checks.
  - Include relevant documentation updates.

---

## 💬 Communication & Collaboration

- Discussions happen in **GitHub Issues** and **Discussions** tabs.
- Be respectful and constructive — follow the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct).
- Use threads for focused discussions.

---

## 🚀 Submitting Your First PR

1. Commit your changes:
   ```bash
   git add .
   git commit -m "feat(core): add watchpoint tracking"
   ```

2. Run the verification checks:
   ```bash
   just fmt-check
   just lint
   just test
   just test-integration
   just check-boundary
   ```

3. Push your branch:
   ```bash
   git push origin feature/my-feature
   ```

4. Open a Pull Request on GitHub:
   - Use a clear title and description.
   - Link to related issues or roadmap items.
   - Ensure CI passes before requesting review.

5. Wait for review and make requested changes.

---

## 🧑‍🤝‍🧑 Contributor Recognition

Contributors are listed in the **README** and on the project site.
Significant contributors may be invited to join the Ferros core team.

---

## 🧾 License

By contributing to Ferros, you agree that your contributions will be licensed under:

```
Apache-2.0
```

---

> 🦀 **Ferros** — built from the same element as Rust.
> Thank you for helping build the future of Rust debugging.
