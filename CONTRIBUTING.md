# 🤝 Contributing to Ferros

Welcome to **Ferros**, the Rust-native debugger built from the same element as Rust itself.
We’re excited that you’re interested in contributing! This document outlines how to set up your environment, submit contributions, and collaborate with the team.

---

## 🧭 Project Overview

**Ferros** is an open-source debugger built for Rust developers.
Its goal is to provide a first-class debugging experience that understands Rust’s unique semantics — ownership, lifetimes, and MIR.

Our architecture includes:
- `ferros-core`: Low-level debugger engine and process control.
- `ferros-cli`: Command-line interface.
- `ferros-dap`: Debug Adapter Protocol integration.
- `ferros-mir`: MIR-level interpretation and analysis.

---

## 🧰 Prerequisites

Before contributing, make sure you have the following installed:

- 🦀 [Rust toolchain](https://www.rust-lang.org/tools/install) (latest stable)
- 🧱 `cargo` and `rustup`
- 🧪 [Git](https://git-scm.com/)
- 🧮 For Linux development: `gcc`, `make`, `libc-dev`, and access to `ptrace`
- 🧑‍💻 Optional for IDE integration: VS Code or Neovim + DAP extensions

To verify your setup:
```bash
rustc --version
cargo --version
```

---

## 🏗️ Getting Started

1. **Fork and clone the repository**
   ```bash
   git clone https://github.com/JamalLyons/ferros.git
   cd ferros
   ```

2. **Create a new branch**
   ```bash
   git checkout -b feature/my-feature
   ```

3. **Build the project**
   ```bash
   cargo build
   ```

4. **Run the tests**
   ```bash
   cargo test
   ```

---

## 📦 Project Structure

```
ferros/
 ├─ ferros-core/      # Core engine and process control
 ├─ ferros-cli/       # Command-line interface
 ├─ ferros-dap/       # Debug Adapter Protocol integration
 ├─ ferros-mir/       # MIR interpretation layer
 ├─ docs/             # Developer and user documentation
 ├─ examples/         # Test binaries for debugging
 ├─ .github/          # CI, issue templates, and workflows
 ├─ Cargo.toml        # Workspace configuration
 └─ ROADMAP.md        # Project goals and milestones
```

---

## 🧩 Types of Contributions

You can contribute in many ways:

- 🐞 **Report bugs:** Use the “Bug report” issue template.
- 🧠 **Propose features:** Open a “Feature request” issue or RFC discussion.
- 💻 **Submit code:** Implement tasks from the roadmap or fix open issues.
- 🧾 **Improve docs:** Clarify, expand, or add new documentation.
- 🧩 **Refactor:** Improve code structure, readability, or performance.

Check [ROADMAP.md](./ROADMAP.md) for ideas on what to work on — many items are marked as `good first issue`.

---

## ✏️ Code Style & Standards

Ferros follows **Rust’s standard style guidelines** enforced by `rustfmt` and `clippy`.

Before committing:
```bash
cargo fmt --all
cargo clippy -- -D warnings
```

### Additional Guidelines
- Prefer explicit types where clarity helps.
- Document public functions with Rustdoc comments (`///`).
- Avoid unsafe code unless absolutely necessary — and document why it’s safe.
- Write tests for new functionality where applicable.
- Keep PRs small and focused (ideally < 300 lines of diff).

---

## 🧪 Testing

All new code should include tests when applicable.

Run all tests:
```bash
cargo test
```

Run tests for a specific crate:
```bash
cargo test -p ferros-core
```

Integration tests are stored under `/tests` and `/examples`.

---

## 🗃️ Commit & PR Guidelines

- Write **clear, descriptive commit messages**:
  ```
  feat(core): add DWARF parser for symbol resolution
  fix(cli): handle SIGTRAP signals gracefully
  docs: update architecture overview
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
  - Pass CI and all tests.
  - Include relevant documentation updates.

---

## 💬 Communication & Collaboration

- Discussions happen in **GitHub Issues** and **Discussions** tabs.
- Be respectful and constructive — follow the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct).
- Use threads for focused discussions.
- Large design changes should start as **RFCs** in `/rfcs`.

---

## 🚀 Submitting Your First PR

1. Commit your changes:
   ```bash
   git add .
   git commit -m "feat(core): implement ptrace attach/detach"
   ```

2. Push your branch:
   ```bash
   git push origin feature/my-feature
   ```

3. Open a Pull Request on GitHub:
   - Use a clear title and description.
   - Link to related issues or roadmap items.
   - Ensure CI passes before requesting review.

4. Wait for review and make requested changes.

---

## 🧑‍🤝‍🧑 Contributor Recognition

Contributors are listed in the **README** and on the project site.
Significant contributors may be invited to join the Ferros core team.

---

## 🧾 License

By contributing to Ferros, you agree that your contributions will be licensed under:

```
MIT OR Apache-2.0
```

This follows the same dual-license model as the Rust project.

---

> 🦀 **Ferros** — built from the same element as Rust.
> Thank you for helping build the future of Rust debugging.
