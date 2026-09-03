//! Thin binary entry point. All behavior lives in the `cli` and
//! `exercise` modules (and, from M1 on, `config` / `llm` / `usage`).

mod cli;
mod exercise;

fn main() {
    cli::run();
}
