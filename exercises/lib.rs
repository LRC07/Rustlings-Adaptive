//! IDE-only crate that wires every exercise up as a module so that
//! rust-analyzer gives full type inference / completion / go-to-def while
//! editing them in VS Code.
//!
//! Seed fixtures below are gated with `#[cfg(rust_analyzer)]`:
//! rust-analyzer sets that cfg during analysis (they get full IDE
//! support), cargo does not (they never break `cargo build` — and as
//! permanently-unsolved samples they must not: see `已知限制`).
//! Generated exercises are wired UNGATED from `lib_generated.rs` so
//! cargo check surfaces borrow-checker errors to the editor (9.5).
//!
//! M4.5a: the eight seed exercises are development fixtures and live
//! under `exercises/fixtures/`; the CLI's default discovery excludes
//! them (`/practice all` shows them). Generated exercises (the learner
//! content) live under `exercises/generated/` and are wired from
//! `lib_generated.rs`.

#[cfg(rust_analyzer)]
#[path = "fixtures/generics/generics1.rs"]
mod generics1;

#[cfg(rust_analyzer)]
#[path = "fixtures/generics/generics2.rs"]
mod generics2;

#[cfg(rust_analyzer)]
#[path = "fixtures/generics/generics3.rs"]
mod generics3;

#[cfg(rust_analyzer)]
#[path = "fixtures/generics/generics4.rs"]
mod generics4;

#[cfg(rust_analyzer)]
#[path = "fixtures/traits/traits1.rs"]
mod traits1;

#[cfg(rust_analyzer)]
#[path = "fixtures/traits/traits2.rs"]
mod traits2;

#[cfg(rust_analyzer)]
#[path = "fixtures/traits/traits3.rs"]
mod traits3;

#[cfg(rust_analyzer)]
#[path = "fixtures/traits/traits4.rs"]
mod traits4;

// Generated exercises (the learner content) are wired from
// `lib_generated.rs` — gitignored, maintained by the generator at
// runtime (the CLI ensures the file exists at startup). Deliberately
// UNGATED: cargo check (and rust-analyzer's flycheck-on-save) must see
// unsolved exercises so borrow-checker errors show up inline, which
// rust-analyzer's own analysis cannot produce. The `exercises` member
// is not in `default-members`, so plain `cargo build` / `cargo test` /
// `cargo run` on the repo root are unaffected. An unsolved exercise
// therefore makes `cargo check -p rustlings-adaptive-exercises` red —
// that is the intended "rustlings experience".
#[path = "lib_generated.rs"]
mod lib_generated;
