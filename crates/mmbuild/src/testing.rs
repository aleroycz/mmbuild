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

//! `mmbuild test` — compile and run the C test programs under `tests/`.
//!
//! Each test file is a standalone program: it `#include`s the module(s) it
//! exercises (so there is no shared link graph to manage), defines `main()`,
//! and returns 0 on success / non-zero on failure. We build each for the host
//! with `zig cc` and run it, cargo-test style, collecting pass/fail and timing.
//!
//! Host-native on purpose: the portable logic (hashing, CRCs, parsers, the
//! SHA-256d share math) is what unit tests should cover, and it runs anywhere.
//! Device-only code (framebuffer, evdev, Linux syscalls) is not meant for this.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::{Settings, TestSpec};
use crate::toolchain::{collect_sources, find_zig};

/// The result of one test program.
pub struct TestOutcome {
    pub name: String,
    pub passed: bool,
    pub compiled: bool,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub ms: u128,
    /// Combined stdout+stderr (compile or run), trimmed for display.
    pub output: String,
}

/// Build + run every matching test. Returns the per-test outcomes in a stable
/// (name-sorted) order; the caller prints the summary and sets the exit code.
pub fn run(
    root: &Path,
    spec: &TestSpec,
    settings: &Settings,
    filter: Option<&str>,
) -> Result<Vec<TestOutcome>> {
    let zig = find_zig(root, settings)?;
    let mut files = collect_sources(root, &spec.include, &[])?;
    files.retain(|f| match filter {
        None => true,
        Some(needle) => f
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.contains(needle))
            .unwrap_or(false),
    });
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let out_dir = resolve(root, &spec.out_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create {}", out_dir.display()))?;

    let exe_ext = if cfg!(windows) { ".exe" } else { "" };
    let timeout = if spec.timeout_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(spec.timeout_secs))
    };

    let mut outcomes = Vec::new();
    for src in &files {
        let name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("test")
            .to_string();
        let exe = out_dir.join(format!("{name}{exe_ext}"));
        let started = Instant::now();

        // ---- compile -------------------------------------------------------
        let mut cmd = Command::new(&zig);
        cmd.arg("cc");
        if !spec.target.is_empty() {
            cmd.arg("-target").arg(&spec.target);
        }
        for f in &spec.cflags {
            cmd.arg(f);
        }
        for inc in &spec.include_dirs {
            cmd.arg("-I").arg(resolve(root, inc));
        }
        cmd.arg(src).arg("-lm").arg("-o").arg(&exe);
        cmd.current_dir(root);
        let build = cmd.output().with_context(|| format!("spawn zig cc for {name}"))?;

        if !build.status.success() {
            let msg = String::from_utf8_lossy(&build.stderr);
            outcomes.push(TestOutcome {
                name,
                passed: false,
                compiled: false,
                code: None,
                timed_out: false,
                ms: started.elapsed().as_millis(),
                output: trim_output(&msg),
            });
            continue;
        }

        // ---- run -----------------------------------------------------------
        let (passed, code, timed_out, output) = run_one(&exe, root, timeout);
        outcomes.push(TestOutcome {
            name,
            passed,
            compiled: true,
            code,
            timed_out,
            ms: started.elapsed().as_millis(),
            output,
        });
    }

    outcomes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(outcomes)
}

/// Run a built test exe with an optional timeout. Output is captured via pipes;
/// test programs are expected to print little, so the OS pipe buffer never fills
/// before we read it after exit.
fn run_one(
    exe: &Path,
    root: &Path,
    timeout: Option<Duration>,
) -> (bool, Option<i32>, bool, String) {
    let mut child = match Command::new(exe)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, None, false, format!("could not start test: {e}")),
    };

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if let Some(t) = timeout {
                    if start.elapsed() > t {
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break None;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break None,
        }
    };

    let mut output = String::new();
    if let Some(mut so) = child.stdout.take() {
        let _ = so.read_to_string(&mut output);
    }
    if let Some(mut se) = child.stderr.take() {
        let mut e = String::new();
        let _ = se.read_to_string(&mut e);
        if !e.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&e);
        }
    }

    let code = status.and_then(|s| s.code());
    let passed = !timed_out && status.map(|s| s.success()).unwrap_or(false);
    (passed, code, timed_out, trim_output(&output))
}

/// Keep test output compact in the summary: last ~12 non-empty lines.
fn trim_output(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = if lines.len() > 12 {
        &lines[lines.len() - 12..]
    } else {
        &lines[..]
    };
    tail.join("\n")
}

fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}

/// Convenience used by `main` to turn outcomes into an exit code.
pub fn all_passed(outcomes: &[TestOutcome]) -> bool {
    outcomes.iter().all(|o| o.passed)
}
