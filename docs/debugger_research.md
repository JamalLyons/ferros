# Rust Debugger Research & Learning Resources

This document provides comprehensive learning resources organized by topic to support building **Ferros**, a Rust-native debugger from scratch. Resources are mapped to the phases in `ROADMAP.md`.

> **How to use this guide**: Start with the prerequisites, then follow resources as you work through each roadmap phase. Mark items as you complete them.

---

## 📚 Essential Books

### Systems Programming & Operating Systems

**Computer Systems: A Programmer's Perspective (CS:APP)** by Bryant & O'Hallaron
- **Chapters to read**: 7 (Memory Hierarchy), 8 (Exceptional Control Flow), 9 (Virtual Memory)
- **Why**: Best resource for understanding process memory layout, signals, and system calls
- **Link**: https://csapp.cs.cmu.edu/

**The Linux Programming Interface** by Michael Kerrisk
- **Chapters to read**: 20 (Signals), 26 (Monitoring Child Processes), 27 (Program Execution)
- **Why**: Deep dive into Linux system calls including `ptrace`
- **Link**: https://man7.org/tlpi/

**Operating Systems: Three Easy Pieces** by Remzi & Andrea Arpaci-Dusseau
- **Chapters to read**: Process API, Virtual Memory sections
- **Why**: Excellent free OS textbook with clear explanations
- **Link**: https://pages.cs.wisc.edu/~remzi/OSTEP/

### Debugging Specific

**Building a Debugger** by O'Reilly (2024)
- **Why**: Modern guide specifically about debugger construction
- **Link**: https://www.oreilly.com/library/view/building-a-debugger/

**The Art of Debugging with GDB, DDD, and Eclipse**
- **Why**: Learn from established debugger workflows
- **Link**: https://nostarch.com/debugging

**Debugging: The 9 Indispensable Rules** by David J. Agans
- **Why**: General debugging philosophy (helpful for debugging your debugger!)
- **Link**: https://debuggingrules.com/

### Low-Level & Assembly

**Programming from the Ground Up** by Jonathan Bartlett
- **Why**: Learn x86-64 assembly and register usage
- **Link**: https://savannah.nongnu.org/projects/pgubook/

**x86-64 Assembly Language Programming with Ubuntu** by Ed Jorgensen
- **Why**: Modern x86-64 assembly reference
- **Link**: http://www.egr.unlv.edu/~ed/x86.html

---

## 🌐 Online Documentation & Guides

### Phase 0: Prerequisites

#### Computer Science Fundamentals

**Process Memory Layout**
- Linux Memory Management: https://www.kernel.org/doc/html/latest/admin-guide/mm/
- Virtual Memory Tutorial: https://www.bottomupcs.com/virtual_memory.xhtml
- Stack and Heap Guide: https://www.learncpp.com/cpp-tutorial/the-stack-and-the-heap/

**CPU Registers & Architecture**
- x86-64 Register Reference: https://wiki.osdev.org/CPU_Registers_x86-64
- Intel Software Developer Manual: https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html
- AMD64 Architecture Reference: https://www.amd.com/en/support/tech-docs

**System Calls & Signals**
- `ptrace(2)` man page: https://man7.org/linux/man-pages/man2/ptrace.2.html
- `waitpid(2)` man page: https://man7.org/linux/man-pages/man2/waitpid.2.html
- Linux Signals Overview: https://man7.org/linux/man-pages/man7/signal.7.html
- `/proc` filesystem: https://man7.org/linux/man-pages/man5/proc.5.html

#### Debugger Theory

**How Debuggers Work - The Debugging Book**
- Interactive tutorial with examples
- Link: https://www.debuggingbook.org/html/Debugger.html
- **Read chapters**: "How Debuggers Work", "Tracking Failure Origins"

**Eli Bendersky's Blog - How Debuggers Work**
- Excellent 3-part series with code examples
- Part 1: https://eli.thegreenplace.net/2011/01/23/how-debuggers-work-part-1
- Part 2: https://eli.thegreenplace.net/2011/01/27/how-debuggers-work-part-2-breakpoints
- Part 3: https://eli.thegreenplace.net/2011/02/07/how-debuggers-work-part-3-debugging-information

**Writing a Linux Debugger (Blog Series)**
- Modern tutorial in C++ with detailed explanations
- Link: https://blog.tartanllama.xyz/writing-a-linux-debugger-setup/
- **Covers**: Setup, breakpoints, registers, source-level stepping, DWARF

**GDB Internals Documentation**
- Architecture of GNU Debugger
- Link: https://sourceware.org/gdb/current/onlinedocs/gdbint/
- **Read sections**: Overall Structure, Symbol Handling, Target Architecture

#### Debug Information Formats

**DWARF Debugging Information Format**
- Official DWARF 5 Standard: http://dwarfstd.org/doc/DWARF5.pdf
- DWARF Introduction Tutorial: https://dwarfstd.org/doc/Debugging%20using%20DWARF-2012.pdf
- DWARF for Beginners: https://www.ibm.com/developerworks/library/os-debugging/
- **Focus on**: Chapters 2 (General Structure), 3 (Program Scope Entries), 6 (Line Number Info)

**ELF Binary Format**
- ELF Specification: https://refspecs.linuxfoundation.org/elf/elf.pdf
- ELF Tutorial: https://wiki.osdev.org/ELF
- Understanding ELF: https://linux-audit.com/elf-binaries-on-linux-understanding-and-analysis/
- Tool: `readelf -a <binary>` to explore binaries

**Symbol Tables and Debug Sections**
- Understanding .debug_* sections: https://sourceware.org/gdb/wiki/FAQ
- Symbol resolution: https://www.akkadia.org/drepper/dsohowto.pdf

---

### Phase 1: Proof of Concept Projects

#### ptrace System Call

**Man Pages & Kernel Docs**
- Official ptrace(2): https://man7.org/linux/man-pages/man2/ptrace.2.html
- Kernel ptrace documentation: https://www.kernel.org/doc/html/latest/admin-guide/LSM/Yama.html
- ptrace requests reference: https://github.com/torvalds/linux/blob/master/include/uapi/linux/ptrace.h

**Tutorials**
- Playing with ptrace: https://www.linuxjournal.com/article/6100
- ptrace Tutorial: https://medium.com/@yangyang.ware/ptrace-tutorial-4b206b48f84d
- Advanced ptrace usage: https://nullprogram.com/blog/2018/06/23/

#### Rust Crates for System Programming

**nix crate (ptrace bindings)**
- Documentation: https://docs.rs/nix/latest/nix/
- ptrace module: https://docs.rs/nix/latest/nix/sys/ptrace/
- Examples: https://github.com/nix-rust/nix/tree/master/test/sys

**libc crate**
- Documentation: https://docs.rs/libc/
- Constants reference: https://docs.rs/libc/latest/libc/

#### Breakpoint Mechanisms

**Software Breakpoints (INT3)**
- Intel Manual Vol 3 (Section 17.4): Breakpoint exception (#BP)
- INT3 instruction: https://c9x.me/x86/html/file_module_x86_id_142.html
- Implementing breakpoints: https://blog.tartanllama.xyz/writing-a-linux-debugger-breakpoints/

**Hardware Breakpoints**
- Debug registers (DR0-DR7): https://wiki.osdev.org/Debug_Registers
- Hardware vs software breakpoints: https://interrupt.memfault.com/blog/cortex-m-breakpoints

---

### Phase 2-3: Core Implementation

#### DWARF Parsing in Rust

**gimli crate**
- Documentation: https://docs.rs/gimli/
- Tutorial: https://github.com/gimli-rs/gimli/blob/master/examples/simple.rs
- Example projects using gimli:
  - addr2line: https://github.com/gimli-rs/addr2line
  - backtrace-rs: https://github.com/rust-lang/backtrace-rs

**object crate**
- Documentation: https://docs.rs/object/
- Reading ELF: https://github.com/gimli-rs/object/blob/master/examples/readobj.rs
- GitHub: https://github.com/gimli-rs/object

**addr2line crate**
- Documentation: https://docs.rs/addr2line/
- Resolve addresses to file:line: https://github.com/gimli-rs/addr2line

**rustc-demangle**
- Documentation: https://docs.rs/rustc-demangle/
- Demangle Rust symbols: https://github.com/rust-lang/rustc-demangle

#### Memory Management & Process Control

**Process Memory**
- /proc/[pid]/maps format: https://man7.org/linux/man-pages/man5/proc.5.html
- Memory regions in Linux: https://www.kernel.org/doc/Documentation/filesystems/proc.txt
- Virtual memory areas: https://www.kernel.org/doc/gorman/html/understand/understand007.html

**Signal Handling**
- signal(7): https://man7.org/linux/man-pages/man7/signal.7.html
- SIGTRAP handling: https://docs.rs/nix/latest/nix/sys/signal/enum.Signal.html
- Signal safety: https://man7.org/linux/man-pages/man7/signal-safety.7.html

---

### Phase 4-6: Advanced Features

#### Stack Unwinding

**DWARF Call Frame Information**
- CFI Tutorial: https://www.imperialviolet.org/2017/01/18/cfi.html
- .eh_frame vs .debug_frame: https://gcc.gnu.org/wiki/Dwarf2EHNewbiesHowto
- Stack unwinding deep dive: https://maskray.me/blog/2020-11-08-stack-unwinding

**Rust Stack Unwinding**
- Unwinding in Rust: https://doc.rust-lang.org/nomicon/unwinding.html
- panic vs abort: https://doc.rust-lang.org/edition-guide/rust-2018/error-handling-and-panics/aborting-on-panic.html

#### Variable Inspection & DWARF Locations

**DWARF Location Expressions**
- Location lists spec: DWARF 5 spec section 2.6
- Expression evaluation: https://dwarfstd.org/doc/DWARF4.pdf (Section 2.5)
- Tutorial: https://sites.google.com/site/x86oracle/debugger-internals/location-expressions

---

### Phase 7: Rust Compiler Internals

#### MIR (Mid-level Intermediate Representation)

**Official Documentation**
- Rust Compiler Dev Guide: https://rustc-dev-guide.rust-lang.org/
- MIR chapter: https://rustc-dev-guide.rust-lang.org/mir/index.html
- MIR construction: https://rustc-dev-guide.rust-lang.org/mir/construction.html
- MIR optimizations: https://rustc-dev-guide.rust-lang.org/mir/optimizations.html

**Viewing MIR**
- `rustc --emit=mir` flag
- MIR playground: https://play.rust-lang.org/ (Click "MIR" button)
- cargo-show-mir: https://github.com/pacak/cargo-show-asm

**rustc_private**
- Using rustc as a library: https://rustc-dev-guide.rust-lang.org/rustc-driver.html
- rustc_interface docs: https://doc.rust-lang.org/nightly/nightly-rustc/rustc_interface/
- Example projects:
  - Miri: https://github.com/rust-lang/miri
  - Clippy: https://github.com/rust-lang/rust-clippy

#### Miri (MIR Interpreter)

**Official Resources**
- Miri Repository: https://github.com/rust-lang/miri
- Miri documentation: https://github.com/rust-lang/miri/blob/master/README.md
- Architecture overview: https://github.com/rust-lang/miri/blob/master/ARCHITECTURE.md

**Papers & Talks**
- "Miri: An Interpreter for Rust's MIR" (RustConf 2018): https://www.youtube.com/watch?v=ri008s0-Rzs
- Stacked Borrows paper: https://plv.mpi-sws.org/rustbelt/stacked-borrows/

#### Rust Ownership & Memory Model

**Official Rust Book**
- Chapter 4 (Ownership): https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html
- Chapter 10 (Lifetimes): https://doc.rust-lang.org/book/ch10-03-lifetime-syntax.html
- Rustonomicon (unsafe Rust): https://doc.rust-lang.org/nomicon/

**Deep Dives**
- Visualizing memory layout: https://rust-unofficial.github.io/too-many-lists/
- Drop order: https://doc.rust-lang.org/reference/destructors.html
- Layout of types: https://doc.rust-lang.org/reference/type-layout.html

---

### Phase 8: IDE Integration

#### Debug Adapter Protocol (DAP)

**Official Specification**
- DAP Protocol: https://microsoft.github.io/debug-adapter-protocol/
- Specification PDF: https://microsoft.github.io/debug-adapter-protocol/specification
- Protocol overview: https://microsoft.github.io/debug-adapter-protocol/overview

**Implementing DAP**
- VS Code debug extension guide: https://code.visualstudio.com/api/extension-guides/debugger-extension
- DAP examples: https://github.com/microsoft/vscode-debugadapter-node
- Rust DAP implementations:
  - CodeLLDB: https://github.com/vadimcn/vscode-lldb
  - rust-analyzer's approach: https://github.com/rust-lang/rust-analyzer

**VS Code Extension Development**
- Extension API: https://code.visualstudio.com/api
- Debug API: https://code.visualstudio.com/api/extension-guides/debugger-extension
- Publishing extensions: https://code.visualstudio.com/api/working-with-extensions/publishing-extension

---

## 🎥 Video Tutorials & Talks

### Debuggers & Systems Programming

**How do debuggers work? (Computerphile)**
- YouTube: https://www.youtube.com/watch?v=Q3klFfS_FqE
- Great high-level overview

**Live Coding: Building a Debugger**
- GitHub: https://github.com/TartanLlama/minidbg
- Blog series companion videos

**Systems Programming**
- MIT 6.828 Operating Systems: https://www.youtube.com/playlist?list=PLfciLKR3SgqNJKKIKUliWoNBBH1VHL3AP
- Watch lectures on virtual memory and processes

### Rust-Specific

**RustConf Talks**
- "Miri: An Interpreter for Rust's MIR": https://www.youtube.com/watch?v=ri008s0-Rzs
- "Rust at Speed - Building a Fast Concurrent Database": https://www.youtube.com/watch?v=s19G6n0UjsM
- Search "RustConf debugger" for more

**Rust Linz Talks**
- Various deep dives into Rust internals: https://www.youtube.com/c/RustLinz

---

## 🛠️ Essential Tools & Crates

### Development Tools

**Binary Analysis**
- `readelf` - Display ELF file information
- `objdump` - Disassemble and analyze object files
- `nm` - List symbols from object files
- `strings` - Find printable strings in files
- `file` - Determine file type
- `hexdump` - View binary data

**Debugging Tools**
- `strace` - Trace system calls
- `ltrace` - Trace library calls
- `gdb` - GNU Debugger (study its behavior)
- `lldb` - LLVM Debugger

**Profiling & Analysis**
- `perf` - Linux profiling tool
- `valgrind` - Memory debugging
- `cargo-flamegraph` - Generate flamegraphs
- `cargo-bloat` - Find what takes space in binary

### Rust Crates by Category

#### Process Control & System Calls
- `nix` (0.27+): POSIX APIs, ptrace bindings
  - Docs: https://docs.rs/nix/
- `libc`: Raw libc bindings
  - Docs: https://docs.rs/libc/

#### Debug Information Parsing
- `gimli` (0.28+): DWARF parser
  - Docs: https://docs.rs/gimli/
- `object` (0.32+): Object file parser (ELF, Mach-O, PE)
  - Docs: https://docs.rs/object/
- `addr2line` (0.21+): Address to source location
  - Docs: https://docs.rs/addr2line/
- `rustc-demangle`: Demangle Rust symbols
  - Docs: https://docs.rs/rustc-demangle/

#### Disassembly
- `capstone` (0.11+): Multi-architecture disassembler
  - Docs: https://docs.rs/capstone/
- `iced-x86` (1.20+): x86/x64 disassembler
  - Docs: https://docs.rs/iced-x86/

#### CLI & UI
- `clap` (4.0+): Command-line argument parsing
  - Docs: https://docs.rs/clap/
- `rustyline` (12.0+): Readline implementation
  - Docs: https://docs.rs/rustyline/
- `ratatui` (0.24+): Terminal UI framework
  - Docs: https://docs.rs/ratatui/
- `colored` (2.0+): Terminal colors
  - Docs: https://docs.rs/colored/

#### Async & Concurrency
- `tokio` (1.33+): Async runtime
  - Docs: https://docs.rs/tokio/
- `async-std`: Alternative async runtime
  - Docs: https://docs.rs/async-std/

#### Error Handling & Logging
- `thiserror` (1.0+): Derive error types
  - Docs: https://docs.rs/thiserror/
- `anyhow` (1.0+): Application error handling
  - Docs: https://docs.rs/anyhow/
- `tracing` (0.1+): Structured logging
  - Docs: https://docs.rs/tracing/
- `tracing-subscriber`: Log formatting
  - Docs: https://docs.rs/tracing-subscriber/

#### Serialization & Protocols
- `serde` (1.0+): Serialization framework
  - Docs: https://docs.rs/serde/
- `serde_json`: JSON support
  - Docs: https://docs.rs/serde_json/

#### Testing
- `criterion` (0.5+): Benchmarking
  - Docs: https://docs.rs/criterion/
- `proptest` (1.4+): Property-based testing
  - Docs: https://docs.rs/proptest/
- `insta` (1.34+): Snapshot testing
  - Docs: https://docs.rs/insta/

---

## 📖 Language-Specific Resources

### Rust Programming

**The Rust Programming Language Book**
- Official book: https://doc.rust-lang.org/book/
- **Key chapters**: 4 (Ownership), 10 (Generics/Lifetimes), 15 (Smart Pointers), 16 (Concurrency)

**Rust by Example**
- Interactive examples: https://doc.rust-lang.org/rust-by-example/
- **Focus on**: Scoping rules, Error handling, Testing

**The Rustonomicon**
- Unsafe Rust: https://doc.rust-lang.org/nomicon/
- **Read**: Ownership, References, FFI, Unwinding

**Rust Reference**
- Language reference: https://doc.rust-lang.org/reference/
- **Sections**: Type layout, Memory model, Attributes

---

## 🔬 Research Papers & Academic Resources

### Debugging & Program Analysis

**Papers**
- "Debugging with DWARF" - Michael J. Eager: http://dwarfstd.org/doc/Debugging%20using%20DWARF-2012.pdf
- "The Implementation of the BRL-CAD Database" (discusses ptrace): https://brlcad.org/papers/
- "Stacked Borrows: An Aliasing Model for Rust": https://plv.mpi-sws.org/rustbelt/stacked-borrows/

**Academic Courses**
- MIT 6.828 Operating Systems: https://pdos.csail.mit.edu/6.828/
- Stanford CS140 Operating Systems: http://web.stanford.edu/~ouster/cgi-bin/cs140-spring20/index.php
- CMU 15-213 Intro to Computer Systems: https://www.cs.cmu.edu/~213/

---

## 👥 Community Resources

### Forums & Discussion

**Rust Community**
- Rust Users Forum: https://users.rust-lang.org/
- Rust Internals Forum: https://internals.rust-lang.org/
- r/rust Subreddit: https://reddit.com/r/rust
- Rust Discord: https://discord.gg/rust-lang

**Specific Channels/Topics**
- #compiler on Rust Discord
- #beginners for general Rust questions
- #project-showcase to share progress

### GitHub Repositories to Study

**Debuggers in Rust**
- probe-rs: https://github.com/probe-rs/probe-rs (embedded debugging)
- gdbstub: https://github.com/daniel5151/gdbstub (GDB stub implementation)
- headcrab: https://github.com/headcrab-rs/headcrab (experimental debugger)

**Tools to Learn From**
- cargo-watch: https://github.com/watchexec/cargo-watch
- cargo-expand: https://github.com/dtolnay/cargo-expand
- cargo-asm: https://github.com/gnzlbg/cargo-asm

---

## 🎯 Practice Projects & Exercises

### Before Starting Ferros

1. **Mini-tracer** (1-2 days)
   - Attach to a process with ptrace
   - Print every system call it makes (like strace)
   - Tutorial: https://blog.nelhage.com/2010/08/write-yourself-an-strace-in-70-lines-of-code/

2. **Symbol Dumper** (2-3 days)
   - Parse ELF file with `object` crate
   - Extract and print all DWARF symbols with `gimli`
   - Map addresses to source lines

3. **Disassembler** (2-3 days)
   - Read instructions from memory
   - Disassemble with `capstone` or `iced-x86`
   - Pretty-print assembly

4. **Stack Walker** (3-4 days)
   - Read stack frames from a stopped process
   - Use DWARF CFI to unwind stack
   - Print backtrace with function names

### Code Katas

- Implement a simple expression evaluator (for watch expressions)
- Write a ptrace-based process monitor
- Build a memory hexdump viewer
- Create a register display formatter

---

## 📋 Checklists

### Before Starting Phase 1

- [ ] Read CS:APP chapters 7-9
- [ ] Complete "How Debuggers Work" tutorial
- [ ] Read Eli Bendersky's debugger blog series
- [ ] Experiment with `readelf` and `objdump` on 5+ binaries
- [ ] Read `ptrace(2)` man page thoroughly
- [ ] Write a hello-world program in Rust and inspect its memory layout

### Before Starting MIR Integration (Phase 8)

- [ ] Read entire MIR chapter of rustc-dev-guide
- [ ] View MIR for 10+ different Rust programs
- [ ] Study Miri's source code for 2-3 hours
- [ ] Understand difference between HIR, MIR, and LLVM IR
- [ ] Set up nightly toolchain
- [ ] Successfully use rustc_private in a toy project

### Before First Release (Phase 12)

- [ ] All core features working and tested
- [ ] Documentation complete
- [ ] 3+ people have successfully used it
- [ ] Performance benchmarks run
- [ ] All clippy warnings resolved
- [ ] Cross-platform CI passing

---

## 🔖 Quick Reference Links

### Man Pages
- ptrace: `man 2 ptrace`
- waitpid: `man 2 waitpid`
- signal: `man 7 signal`
- proc: `man 5 proc`
- elf: `man 5 elf`

### Rust Documentation
- std::process: https://doc.rust-lang.org/std/process/
- std::ptr: https://doc.rust-lang.org/std/ptr/
- Rust Reference: https://doc.rust-lang.org/reference/

### Specifications
- DWARF 5: http://dwarfstd.org/doc/DWARF5.pdf
- ELF: https://refspecs.linuxfoundation.org/elf/elf.pdf
- x86-64 ABI: https://gitlab.com/x86-psABIs/x86-64-ABI

### Online Tools
- Compiler Explorer: https://godbolt.org/ (see generated assembly)
- Rust Playground: https://play.rust-lang.org/ (experiment with code)
- crates.io: https://crates.io/ (find Rust packages)
- docs.rs: https://docs.rs/ (crate documentation)

---

## 💡 Tips for Effective Learning

### Reading Strategy
1. **Skim first**: Get overview of chapter/doc
2. **Deep read**: Focus on one section at a time
3. **Take notes**: Summarize in your own words
4. **Try examples**: Run code, modify it, break it
5. **Teach others**: Explain concepts to solidify understanding

### When Stuck
1. Read the error message carefully
2. Search the issue on GitHub/Stack Overflow
3. Ask in Rust Discord #beginners
4. Create a minimal reproducible example
5. Take a break and come back fresh

### Staying Organized
- Keep a learning journal (markdown file works great)
- Mark resources as you complete them
- Note interesting discoveries
- Track blockers and questions
- Celebrate completed milestones!

---

## 📅 Suggested Learning Timeline

**Weeks 1-2: Prerequisites**
- Computer science fundamentals
- Debugger theory
- DWARF basics

**Weeks 3-4: Hands-on POCs**
- Phase 1 proof-of-concept projects
- Experiment with ptrace
- Parse your first DWARF symbols

**Weeks 5-12: Core Implementation**
- ferros-core (process control)
- ferros-protocol (symbols)
- ferros-utils

**Weeks 13-16: CLI & Features**
- Command-line interface
- Stack unwinding
- Variable inspection

**Weeks 17-20: Polish & Release**
- Testing
- Documentation
- First release!

**Note**: This is flexible! Go at your own pace. Some phases might take longer, others might be faster.

---

## 🎓 Additional Learning Paths

### If You Want Deeper OS Knowledge
- Take MIT 6.828 (OS Engineering): https://pdos.csail.mit.edu/6.828/
- Read "Linux Kernel Development" by Robert Love
- Study kernel source: https://github.com/torvalds/linux

### If You Want Compiler Expertise
- Take Stanford CS143 (Compilers): https://web.stanford.edu/class/cs143/
- Read "Crafting Interpreters": https://craftinginterpreters.com/
- Study rust-analyzer source code

### If You Want Debugging Theory
- Study symbolic execution (angr, KLEE)
- Learn about constraint solvers (Z3)
- Research program analysis techniques

---

**Remember**: You don't need to read everything before starting! Use this as a reference guide. Come back to sections as you need them during development.

**Next Step**: Start with the Prerequisites section and work through Phase 1 POC projects in `ROADMAP.md`! 🚀
