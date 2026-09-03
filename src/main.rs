//! Thin binary entry point. All behavior lives in the `cli`, `exercise`,
//! `config`, `constraints`, `llm`, `usage`, and `verifier` modules.

mod cli;
mod config;
// M2 modules are exercised by unit tests today; the CLI wires them up in
// M3 (templates/generator) and M4 (agent loop). Silence dead_code until
// then.
#[allow(dead_code)]
mod constraints;
mod exercise;
mod llm;
mod usage;
#[allow(dead_code)]
mod verifier;

fn main() {
    cli::run();
}
