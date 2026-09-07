//! IDE-only crate that wires every exercise up as a module so that
//! rust-analyzer gives full type inference / completion / go-to-def while
//! editing them in VS Code.
//!
//! Generated exercises are wired UNGATED from `lib_generated.rs` so
//! cargo check surfaces borrow-checker errors to the editor (9.5).
//!
//! History: the repo used to ship eight seed exercises under
//! `exercises/fixtures/` (IDE-gated with `#[cfg(rust_analyzer)]`).
//! They were internal development samples, not learner content —
//! four even carried complete solutions and auto-passed on open
//! (0907 实测反馈 P1) — so they were removed; the learner content is
//! generated exercises only. The `fixtures/` discovery exclusion in
//! `src/exercise/mod.rs` stays: a user-local `exercises/fixtures/`
//! directory is still treated as non-learner data.
//!
//! The `exercises` member is not in `default-members`, so plain
//! `cargo build` / `cargo test` / `cargo run` on the repo root are
//! unaffected. An unsolved exercise makes `cargo check -p
//! rustlings-adaptive-exercises` red — that is the intended
//! "rustlings experience".

#[path = "lib_generated.rs"]
mod lib_generated;
