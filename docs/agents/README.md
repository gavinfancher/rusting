# Agent rules (read before your brief)

You are one of several agents building this project in parallel, each in its own git worktree. The overall plan is in `docs/CONTRACTS.md` (interfaces) plus your brief. The project owner is **new to Rust** and wants to learn from this code and run it as a sysadmin, so:

1. **Stay in your lane.** Only create or edit the paths your brief says you own. If you need something outside them, stub it locally or write it under "Proposed changes" in `docs/CONTRACTS.md` (the one shared file you may append to, only in that section) and mention it in your final report.
2. **Contracts are fixed.** Don't change `proto/hearts.proto`, `migrations/0001_init.sql`, `crates/hearts-core/src/{card,types,seed}.rs`, `crates/hearts-proto`, or the HTTP/metrics/env contracts. Build against them.
3. **Dependencies:** add them only to *your* crate's `Cargo.toml` (use `cargo add -p <crate> <dep>` so you get current versions). Don't edit the root `Cargo.toml`. `Cargo.lock` conflicts get resolved at merge time.
4. **Code style:** idiomatic stable Rust (edition 2024, toolchain 1.99), with module-level `//!` docs explaining *what and why*, and comments on anything non-obvious (ownership tricks, concurrency, SQL). Use `thiserror` in libraries and `anyhow` in binaries. No `unwrap()` on anything that can fail at runtime in binaries.
5. **Toolchain:** run `export PATH=/opt/homebrew/opt/rustup/bin:$PATH` first. `protoc` and Docker are installed.
6. **Definition of done:** your brief's acceptance commands pass, and `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` are clean (when touching Rust).
7. **Commit** your work to your worktree's branch with clear messages, ending each with the line `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`. Don't push and don't touch `main`.
8. **Never** apply anything to the real Kubernetes cluster, push images, or create GitHub resources. The orchestrator does that.
9. **Final report:** what you built, how to run it, the acceptance results (paste the test summary), any contract proposals, and known gaps.
