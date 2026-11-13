// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Task lifecycle console with optional TTY progress bar.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Log verbosity (quiet / default / info / debug).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Quiet = 0,
    Lifecycle = 1,
    Info = 2,
    Debug = 3,
}

/// Task completion outcome shown on the lifecycle line.
#[derive(Clone, Copy, Debug)]
pub enum Outcome {
    Executed,
    UpToDate,
    Skipped,
    Failed,
}

impl Outcome {
    fn suffix(self) -> &'static str {
        match self {
            Outcome::Executed => "",
            Outcome::UpToDate => " UP-TO-DATE",
            Outcome::Skipped => " SKIPPED",
            Outcome::Failed => " FAILED",
        }
    }
}

// ---- ANSI (raw; Win11 + Linux terminals support VT) --------------------------
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GREY: &str = "\x1b[90m";
}

struct Op {
    label: String,
    /// Optional (current, total) for byte/step progress within the op.
    progress: Option<(u64, u64)>,
}

struct State {
    level: Level,
    color: bool,
    tty: bool,
    phase: String,
    started: Instant,
    total_tasks: u32,
    done_tasks: u32,
    /// The operation currently shown in the live area (innermost running task).
    active: Option<Op>,
    /// Status-area line count for erase-before-print.
    drawn_lines: u16,
    has_warnings: bool,
    has_errors: bool,
}

/// Cloneable logger; background thread repaints status.
#[derive(Clone)]
pub struct Logger {
    inner: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl Logger {
    pub fn init(level: Level) -> Logger {
        let tty = std::io::stdout().is_terminal();
        let color = tty && std::env::var_os("NO_COLOR").is_none();
        let state = State {
            level,
            color,
            tty,
            phase: "STARTING".into(),
            started: Instant::now(),
            total_tasks: 0,
            done_tasks: 0,
            active: None,
            drawn_lines: 0,
            has_warnings: false,
            has_errors: false,
        };
        let logger = Logger {
            inner: Arc::new(Mutex::new(state)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        if tty {
            let l = logger.clone();
            std::thread::spawn(move || {
                while !l.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(120));
                    let mut st = l.inner.lock().unwrap();
                    if st.active.is_some() {
                        redraw(&mut st);
                    }
                }
            });
        }
        logger
    }

    /// Declare the number of actionable tasks up front so the bar can fill.
    pub fn plan(&self, total_tasks: u32) {
        let mut st = self.inner.lock().unwrap();
        st.total_tasks = total_tasks;
    }

    pub fn set_phase(&self, phase: &str) {
        let mut st = self.inner.lock().unwrap();
        st.phase = phase.to_string();
        redraw(&mut st);
    }

    /// Begin an actionable task; shows `> :label` in the live area.
    pub fn task_start(&self, label: &str) {
        let mut st = self.inner.lock().unwrap();
        st.active = Some(Op {
            label: label.to_string(),
            progress: None,
        });
        redraw(&mut st);
    }

    /// Update the current op's sub-detail (e.g. the file being written).
    pub fn detail(&self, detail: &str) {
        let mut st = self.inner.lock().unwrap();
        if let Some(op) = st.active.as_mut() {
            // keep the task prefix, replace trailing detail
            let base = op
                .label
                .split(" > ")
                .next()
                .unwrap_or(&op.label)
                .to_string();
            op.label = if detail.is_empty() {
                base
            } else {
                format!("{base} > {detail}")
            };
        }
        redraw(&mut st);
    }

    /// Report byte/step progress for the current op (shown as `41% (x/y)`).
    pub fn op_progress(&self, current: u64, total: u64) {
        let mut st = self.inner.lock().unwrap();
        if let Some(op) = st.active.as_mut() {
            op.progress = Some((current, total));
        }
        redraw(&mut st);
    }

    /// Finish the current task, print its permanent completion line, advance bar.
    pub fn task_done(&self, label: &str, outcome: Outcome) {
        let mut st = self.inner.lock().unwrap();
        st.done_tasks = st.done_tasks.saturating_add(1);
        if matches!(outcome, Outcome::Failed) {
            st.has_errors = true;
        }
        let colored = if st.color {
            let c = match outcome {
                Outcome::Executed => ansi::GREEN,
                Outcome::UpToDate | Outcome::Skipped => ansi::YELLOW,
                Outcome::Failed => ansi::RED,
            };
            format!(
                "{bold}> Task {reset}{c}:{label}{suf}{reset}",
                bold = ansi::BOLD,
                reset = ansi::RESET,
                c = c,
                label = label,
                suf = outcome.suffix()
            )
        } else {
            format!("> Task :{label}{}", outcome.suffix())
        };
        st.active = None;
        emit_permanent(&mut st, &colored);
    }

    pub fn lifecycle(&self, msg: &str) {
        let mut st = self.inner.lock().unwrap();
        if st.level >= Level::Lifecycle {
            emit_permanent(&mut st, msg);
        }
    }

    pub fn info(&self, msg: &str) {
        let mut st = self.inner.lock().unwrap();
        if st.level >= Level::Info {
            let line = if st.color {
                format!("{}{}{}", ansi::GREY, msg, ansi::RESET)
            } else {
                msg.to_string()
            };
            emit_permanent(&mut st, &line);
        }
    }

    pub fn debug(&self, msg: &str) {
        let mut st = self.inner.lock().unwrap();
        if st.level >= Level::Debug {
            let line = if st.color {
                format!("{}[debug] {}{}", ansi::DIM, msg, ansi::RESET)
            } else {
                format!("[debug] {msg}")
            };
            emit_permanent(&mut st, &line);
        }
    }

    pub fn warn(&self, msg: &str) {
        let mut st = self.inner.lock().unwrap();
        st.has_warnings = true;
        let line = if st.color {
            format!("{}warning: {}{}", ansi::YELLOW, msg, ansi::RESET)
        } else {
            format!("warning: {msg}")
        };
        emit_permanent(&mut st, &line);
    }

    pub fn error(&self, msg: &str) {
        let mut st = self.inner.lock().unwrap();
        st.has_errors = true;
        let line = if st.color {
            format!("{}{}error: {}{}", ansi::BOLD, ansi::RED, msg, ansi::RESET)
        } else {
            format!("error: {msg}")
        };
        emit_permanent(&mut st, &line);
    }

    /// Footer line: `BUILD SUCCESSFUL in 1m 3s` or `BUILD FAILED in <duration>`.
    pub fn finish(&self, success: bool) {
        let mut st = self.inner.lock().unwrap();
        self.stop.store(true, Ordering::Relaxed);
        clear_status(&mut st);
        let elapsed = crate::human_duration(st.started.elapsed());
        let (word, c) = if success && !st.has_errors {
            ("BUILD SUCCESSFUL", ansi::GREEN)
        } else {
            ("BUILD FAILED", ansi::RED)
        };
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out);
        if st.color {
            let _ = writeln!(
                out,
                "{}{}{} in {}{}",
                ansi::BOLD,
                c,
                word,
                elapsed,
                ansi::RESET
            );
        } else {
            let _ = writeln!(out, "{word} in {elapsed}");
        }
        if st.total_tasks > 0 {
            let _ = writeln!(
                out,
                "{} actionable task{}: {} executed",
                st.total_tasks,
                if st.total_tasks == 1 { "" } else { "s" },
                st.done_tasks
            );
        }
        let _ = out.flush();
    }
}

// ---- rendering ---------------------------------------------------------------

fn emit_permanent(st: &mut State, line: &str) {
    clear_status(st);
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
    redraw(st);
}

fn clear_status(st: &mut State) {
    if !st.tty || st.drawn_lines == 0 {
        st.drawn_lines = 0;
        return;
    }
    let mut out = std::io::stdout().lock();
    // Move to start of current line and clear it, then each line above.
    let _ = write!(out, "\r\x1b[2K");
    for _ in 1..st.drawn_lines {
        let _ = write!(out, "\x1b[1A\x1b[2K");
    }
    let _ = out.flush();
    st.drawn_lines = 0;
}

fn redraw(st: &mut State) {
    if !st.tty {
        return;
    }
    clear_status(st);
    let width = crossterm::terminal::size()
        .map(|(w, _)| w)
        .unwrap_or(80)
        .min(120);
    let mut lines: Vec<String> = Vec::new();

    // Bar line: <====----> NN% PHASE [elapsed]
    let frac = if st.total_tasks == 0 {
        0.0
    } else {
        (st.done_tasks as f64 / st.total_tasks as f64).clamp(0.0, 1.0)
    };
    let pct = (frac * 100.0).round() as u32;
    let bar_w: usize = 22;
    let filled = (frac * bar_w as f64).round() as usize;
    let mut bar = String::with_capacity(bar_w + 2);
    bar.push('<');
    for i in 0..bar_w {
        bar.push(if i < filled { '=' } else { '-' });
    }
    bar.push('>');
    let elapsed = crate::human_duration(st.started.elapsed());
    let barline = if st.color {
        format!(
            "{cyan}{bar}{reset} {bold}{pct:>3}%{reset} {phase} {grey}[{elapsed}]{reset}",
            cyan = ansi::CYAN,
            reset = ansi::RESET,
            bold = ansi::BOLD,
            grey = ansi::GREY,
            bar = bar,
            pct = pct,
            phase = st.phase,
            elapsed = elapsed
        )
    } else {
        format!("{bar} {pct:>3}% {} [{elapsed}]", st.phase)
    };
    lines.push(barline);

    // Active op line.
    if let Some(op) = st.active.as_ref() {
        let prog = match op.progress {
            Some((c, t)) if t > 0 => {
                let p = ((c as f64 / t as f64) * 100.0).round() as u32;
                format!(
                    "  {p}% ({}/{})",
                    crate::human_bytes(c),
                    crate::human_bytes(t)
                )
            }
            _ => String::new(),
        };
        let raw = format!("> {}{}", op.label, prog);
        let clipped = clip(&raw, width as usize);
        let opline = if st.color {
            format!("{}{}{}", ansi::DIM, clipped, ansi::RESET)
        } else {
            clipped
        };
        lines.push(opline);
    }

    let mut out = std::io::stdout().lock();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        let _ = write!(out, "{l}");
    }
    let _ = out.flush();
    st.drawn_lines = lines.len() as u16;
}

/// Clip a display string to `max` columns (ANSI-free input assumed here).
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    let keep: String = s.chars().take(max - 1).collect();
    format!("{keep}…")
}
