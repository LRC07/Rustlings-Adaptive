//! Thin binary entry point. All behavior lives in the `cli`, `exercise`,
//! `config`, `constraints`, `llm`, `usage`, `verifier`, `template`,
//! `taxonomy`, and `generator` modules.

mod cli;
mod config;
mod constraints;
mod exercise;
mod generator;
mod llm;
mod taxonomy;
mod template;
mod usage;
mod verifier;

fn main() {
    cli::run();
}
