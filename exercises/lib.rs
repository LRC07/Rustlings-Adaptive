//! IDE-only crate that wires every exercise up as a module so that
//! rust-analyzer gives full type inference / completion / go-to-def while
//! editing them in VS Code.
//!
//! Every `mod` below is gated with `#[cfg(rust_analyzer)]`. rust-analyzer
//! sets that cfg during analysis, so it sees (and analyzes) all exercises.
//! `cargo` does NOT set it, so to cargo this crate is an empty library —
//! meaning the intentionally-broken exercise templates never break
//! `cargo build` / `cargo test` / `cargo run`. The CLI itself compiles each
//! exercise directly with `rustc --test`, independent of this crate.
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

// Generated exercises are wired from `lib_generated.rs` (gitignored —
// maintained by the generator at runtime, user-local only). Before the
// first `g` generation this file does not exist yet, which shows up as
// a cosmetic "unresolved module" hint in rust-analyzer only.
#[cfg(rust_analyzer)]
#[path = "lib_generated.rs"]
mod lib_generated;
