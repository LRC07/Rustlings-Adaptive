//! Thin binary entry point. All behavior lives in the `cli`, `agent`,
//! `exercise`, `config`, `constraints`, `llm`, `profile`, `review`,
//! `usage`, `verifier`, `template`, `taxonomy`, and `generator` modules.

mod agent;
mod borrowlab;
mod cli;
mod config;
mod constraints;
mod exercise;
mod generator;
mod llm;
mod profile;
mod review;
mod taxonomy;
mod template;
mod usage;
mod verifier;

fn main() {
    // Stdout can disappear under us (SSH drop, closed pipe, terminal
    // window closed while a turn is streaming) — println! then panics
    // with EIO. Catch it here so the RAII terminal guards unwind
    // cleanly and the process exits with the usual panic code instead
    // of tearing down mid-render.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cli::run));
    if result.is_err() {
        std::process::exit(101);
    }
}
