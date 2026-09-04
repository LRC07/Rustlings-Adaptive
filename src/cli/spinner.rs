//! Progress spinner (R4): long tasks (LLM calls, generation, compile)
//! render a live status line with elapsed seconds; the global
//! interrupt flag makes it exit silently so the main loop can report
//! the interruption and return to the prompt.
//!
//! On non-tty stdout (tests, logs) only a single start line is printed
//! so captured output stays readable.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::agent;

/// Shared status text the worker updates through its progress closure.
pub type StatusSlot = Arc<Mutex<String>>;

const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const TICK: Duration = Duration::from_millis(100);

pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Start rendering `label` (tty only; non-tty prints one line).
    pub fn start(label: &str) -> (Self, StatusSlot) {
        let tty = std::io::stdout().is_terminal();
        let stop = Arc::new(AtomicBool::new(false));
        let status: StatusSlot = Arc::new(Mutex::new(label.to_string()));
        let handle = if tty {
            let stop2 = stop.clone();
            let status2 = status.clone();
            Some(std::thread::spawn(move || render(stop2, status2)))
        } else {
            println!("  [进度] {label}");
            None
        };
        (Self { stop, handle }, status)
    }

    /// Stop rendering and clear the status line.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn render(stop: Arc<AtomicBool>, status: StatusSlot) {
    let start = Instant::now();
    let mut i = 0;
    let mut first = true;
    while !stop.load(Ordering::SeqCst) && !agent::is_interrupted() {
        let secs = start.elapsed().as_secs();
        let text = status.lock().map(|s| s.clone()).unwrap_or_default();
        // First frame moves to a fresh line so the spinner never eats
        // the just-echoed input line ("你> …"); ESC[0K wipes the rest
        // of the line so a shorter status leaves no residue. The text
        // is clamped to the terminal width — a wrapped line would
        // break the in-place redraw and flood the screen with one
        // stale row per frame (9.4 实测："生成练习第几轮"刷屏).
        let tw = super::render::term_width();
        let text = super::render::truncate_display(&text, tw.saturating_sub(12));
        if first {
            println!();
            first = false;
        }
        print!("\r  {} {text} {secs:>3}s\x1B[0K", FRAMES[i % FRAMES.len()]);
        let _ = std::io::stdout().flush();
        i += 1;
        std::thread::sleep(TICK);
    }
    // Erase the spinner line entirely.
    print!("\r\x1B[2K");
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_starts_and_stops() {
        // In tests stdout is not a tty: no render thread, just a line.
        let (sp, slot) = Spinner::start("测试中…");
        *slot.lock().unwrap() = "换了状态".to_string();
        sp.stop();
    }
}
