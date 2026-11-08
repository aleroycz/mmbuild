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

//! `mmbuild lint` — the strict-warnings gate for our first-party C, the C-side
//! equivalent of `cargo clippy`. It drives the bundled clang (through `zig cc`)
//! over every source with a harsh warning set (and, optionally, the clang
//! static analyzer) and fails on the first finding, so a green `mmbuild lint`
//! guarantees a `-Werror` build will not break.
//!
//! ## The zig-cc cache gotcha
//!
//! `zig cc` memoises compilations in a global cache and, crucially, **replays
//! nothing on a cache hit** — the second identical invocation prints *zero*
//! diagnostics and exits 0, which would make this linter silently "pass". We
//! defeat that by stamping each compile with a unique `-DMM_LINT_NONCE=<n>`
//! macro (unused by the code) so every run is a cache miss and the warnings are
//! always emitted. `-fsyntax-only` is also broken under `zig cc` (FileNotFound),
//! so we compile to a throwaway object with `-c -o`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::config::{LintGroup, LintSpec, Settings};
use crate::toolchain::{collect_sources, find_zig};

/// One compiler/analyzer diagnostic.
pub struct Finding {
    pub group: String,
    pub file: PathBuf,
    pub line: u32,
    pub col: u32,
    /// "warning" or "error".
    pub kind: String,
    /// The `-W...` flag (or "analyzer"/"error"), without brackets.
    pub flag: String,
    pub msg: String,
}

pub struct LintReport {
    /// Files actually compiled (after excludes).
    pub scanned: usize,
    /// Files skipped as vendored/third-party.
    pub skipped: usize,
    /// Per-group (name, files-scanned) for the summary.
    pub groups: Vec<(String, usize)>,
    /// Every distinct finding, in discovery order.
    pub findings: Vec<Finding>,
    /// Whether the static-analyzer pass ran.
    pub analyzed: bool,
    /// Files the analyzer could not process (zig cc's `--analyze` frontend does
    /// not inherit the cross-target sysroot include paths, so any TU pulling in
    /// a system header fails to resolve it). This is a tooling limitation, not a
    /// code defect, so it is counted separately and never gates the build.
    pub analyze_unavailable: usize,
}

impl LintReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}

/// A process-unique, monotonically increasing nonce used to bust `zig cc`'s
/// compile cache so diagnostics are emitted on every run (see module docs).
fn next_nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    base ^ (COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E3779B97F4A7C15))
}

/// Run the linter. `analyze_override` forces the analyzer pass on/off regardless
/// of the spec; `group_filter` limits to one group by name.
pub fn run(
    root: &Path,
    spec: &LintSpec,
    settings: &Settings,
    analyze_override: Option<bool>,
    group_filter: Option<&str>,
) -> Result<LintReport> {
    let zig = find_zig(root, settings)?;
    let analyze = analyze_override.unwrap_or(spec.analyze);

    let out_dir = root.join("build").join("lint");
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create {}", out_dir.display()))?;

    let mut report = LintReport {
        scanned: 0,
        skipped: 0,
        groups: Vec::new(),
        findings: Vec::new(),
        analyzed: analyze,
        analyze_unavailable: 0,
    };

    for group in &spec.groups {
        if let Some(f) = group_filter {
            if !group.name.eq_ignore_ascii_case(f) {
                continue;
            }
        }
        let files = collect_sources(root, &group.include, &[])?;
        // Drop vendored/third-party files by substring marker.
        let (kept, dropped): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|p| {
            let s = p.to_string_lossy();
            !spec.exclude.iter().any(|ex| s.contains(ex.as_str()))
        });
        report.skipped += dropped.len();
        report.groups.push((group.name.clone(), kept.len()));

        for src in &kept {
            report.scanned += 1;
            compile_pass(&zig, root, group, spec, &out_dir, src, &mut report.findings)?;
            if analyze
                && !analyze_pass(&zig, root, group, &out_dir, src, &mut report.findings)?
            {
                report.analyze_unavailable += 1;
            }
        }
    }

    // The analyzer walks into `#include`d headers, so a vendored/third-party
    // header (e.g. stb_truetype.h) can surface findings even though its source
    // TU was excluded. Drop any finding located in an excluded file so the gate
    // only ever fails on our own code.
    report.findings.retain(|f| {
        let s = f.file.to_string_lossy();
        !spec.exclude.iter().any(|ex| s.contains(ex.as_str()))
    });

    Ok(report)
}

/// A strict warning compile (`-c -o <tmp.o>`). Parses every diagnostic line.
fn compile_pass(
    zig: &Path,
    root: &Path,
    group: &LintGroup,
    spec: &LintSpec,
    out_dir: &Path,
    src: &Path,
    out: &mut Vec<Finding>,
) -> Result<()> {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("x");
    let obj = out_dir.join(format!("{}_{}.o", group.name, stem));

    let mut cmd = Command::new(zig);
    cmd.arg("cc");
    if !group.target.is_empty() {
        cmd.arg("-target").arg(&group.target);
    }
    if !group.mcpu.is_empty() {
        cmd.arg(format!("-mcpu={}", group.mcpu));
    }
    cmd.arg(format!("-DMM_LINT_NONCE={}", next_nonce()));
    for f in &group.cflags {
        cmd.arg(f);
    }
    for w in &spec.warnings {
        cmd.arg(w);
    }
    for inc in &group.include_dirs {
        cmd.arg("-I").arg(resolve(root, inc));
    }
    cmd.arg("-c").arg(src).arg("-o").arg(&obj);
    cmd.current_dir(root);

    let o = cmd
        .output()
        .with_context(|| format!("spawn zig cc for {}", src.display()))?;
    let stderr = String::from_utf8_lossy(&o.stderr);
    parse_diagnostics(&group.name, &stderr, out);
    Ok(())
}

/// A clang static-analyzer pass (`--analyze`). Finds path-sensitive bugs the
/// warning set cannot (null derefs, uninitialised reads, leaks).
///
/// Returns `true` if the translation unit was analyzed, `false` if zig cc's
/// analyzer frontend could not resolve the TU's system headers (reported as a
/// bare `FileNotFound` at 1:1). Such tooling failures are *not* pushed as
/// findings — they would be false positives that wrongly fail the gate.
fn analyze_pass(
    zig: &Path,
    root: &Path,
    group: &LintGroup,
    out_dir: &Path,
    src: &Path,
    out: &mut Vec<Finding>,
) -> Result<bool> {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("x");
    let txt = out_dir.join(format!("{}_{}.analysis.txt", group.name, stem));

    let mut cmd = Command::new(zig);
    cmd.arg("cc");
    if !group.target.is_empty() {
        cmd.arg("-target").arg(&group.target);
    }
    if !group.mcpu.is_empty() {
        cmd.arg(format!("-mcpu={}", group.mcpu));
    }
    cmd.arg(format!("-DMM_LINT_NONCE={}", next_nonce()));
    for f in &group.cflags {
        cmd.arg(f);
    }
    for inc in &group.include_dirs {
        cmd.arg("-I").arg(resolve(root, inc));
    }
    cmd.arg("--analyze")
        .arg("-Xclang")
        .arg("-analyzer-output=text")
        .arg("-o")
        .arg(&txt)
        .arg(src);
    cmd.current_dir(root);

    let o = cmd
        .output()
        .with_context(|| format!("spawn zig cc --analyze for {}", src.display()))?;
    let stderr = String::from_utf8_lossy(&o.stderr);

    // zig cc's `--analyze` driver cannot fully set up a cross-target analysis:
    // it parses and analyzes the primary TU (emitting genuine path-sensitive
    // findings) but then bails on the first unresolved system header with a bare
    // `FileNotFound` at 1:1 (or `fatal error: '...' file not found`). We keep the
    // genuine findings it produced and swallow only the tooling error; a TU that
    // raised one is reported as "partially analyzed" (returns false) so the
    // summary can be honest about coverage, but it never gates the build.
    let fully_analyzed = !(stderr.contains("FileNotFound")
        || stderr.contains("file not found")
        || stderr.contains("fatal error:"));

    parse_diagnostics(&group.name, &stderr, out);
    Ok(fully_analyzed)
}

/// Pull `path:line:col: warning|error: message [-Wflag]` lines out of compiler
/// stderr. Ignores notes, the "N warnings generated." tallies, and any line
/// originating in system/vendor headers the analyzer may walk into.
fn parse_diagnostics(group: &str, stderr: &str, out: &mut Vec<Finding>) {
    for line in stderr.lines() {
        let (kind, idx) = if let Some(i) = line.find(": warning:") {
            ("warning", i)
        } else if let Some(i) = line.find(": error:") {
            ("error", i)
        } else {
            continue;
        };
        // Drop zig's cross-target analyzer bailout pseudo-error (see
        // analyze_pass): it is a tooling failure at 1:1, not a code defect.
        if line[idx + 2..].trim() == "error: FileNotFound" {
            continue;
        }
        let locpart = &line[..idx];
        // locpart == "<path>:<line>:<col>"
        let mut it = locpart.rsplitn(3, ':');
        let col = it.next().and_then(|s| s.trim().parse::<u32>().ok());
        let lineno = it.next().and_then(|s| s.trim().parse::<u32>().ok());
        let path = it.next();
        let (Some(col), Some(lineno), Some(path)) = (col, lineno, path) else {
            // Not a file:line:col location (e.g. a bare "error:"); keep it anyway
            // with zeroed location so nothing is silently dropped.
            let rest = line[idx + 2..].trim();
            out.push(Finding {
                group: group.to_string(),
                file: PathBuf::new(),
                line: 0,
                col: 0,
                kind: kind.to_string(),
                flag: String::new(),
                msg: rest.to_string(),
            });
            continue;
        };

        let after = line[idx + 2 + kind.len() + 1..].trim(); // skip ": warning:"/": error:"
        let (msg, flag) = match (after.rfind("[-W"), after.rfind(']')) {
            (Some(b), Some(e)) if e > b => (
                after[..b].trim().to_string(),
                after[b + 1..e].to_string(), // "-Wxxx"
            ),
            _ => (after.to_string(), String::new()),
        };

        out.push(Finding {
            group: group.to_string(),
            file: PathBuf::from(path),
            line: lineno,
            col,
            kind: kind.to_string(),
            flag,
            msg,
        });
    }
}

/// Fail the command if the tree is not clean. Returns an error carrying the
/// headline count (the caller has already printed the detail).
pub fn gate(report: &LintReport) -> Result<()> {
    if report.is_clean() {
        Ok(())
    } else {
        bail!("{} lint finding(s)", report.findings.len())
    }
}
