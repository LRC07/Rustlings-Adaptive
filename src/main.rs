//! Thin binary entry point. All behavior lives in the `cli`, `exercise`,
//! `config`, `llm`, and `usage` modules.

mod cli;
mod config;
mod exercise;
mod llm;
mod usage;

fn main() {
    cli::run();
}
