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

//! `mmbuild fmt` — a whitespace-level C formatter and linter, in the spirit of
//! `cargo fmt`. It deliberately does NOT reflow or reindent code (that needs a
//! real parser and risks changing meaning); instead it applies only transforms
//! that are always safe and that reviewers otherwise waste time on:
//!
//!   * CRLF / lone-CR line endings  -> LF
//!   * trailing whitespace          -> stripped
//!   * leading tabs                 -> spaces (tab stops at `indent_width`)
//!   * runs of blank lines          -> collapsed to `max_blank_lines`
//!   * blank lines at BOF/EOF       -> removed
//!   * missing / extra final newline-> exactly one
//!
//! The linter additionally *reports* (without changing) lines longer than
//! `max_line_len`. Run with `--check` for a read-only CI gate that fails if any
//! file is not already formatted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::FormatSpec;
use crate::toolchain::collect_sources;

/// What a format run found/did.
pub struct FmtReport {
    pub scanned: usize,
    /// Files that changed (or would change, in check mode).
    pub changed: Vec<PathBuf>,
    /// Non-blocking lint notes: (file, line number, message).
    pub lint: Vec<(PathBuf, usize, String)>,
    /// Files skipped because they are not valid UTF-8 text.
    pub skipped: Vec<PathBuf>,
}

/// Format every matching source. In `check` mode nothing is written and
/// `changed` lists the files that would change.
pub fn run(root: &Path, spec: &FormatSpec, check: bool) -> Result<FmtReport> {
    let files = collect_sources(root, &spec.include, &[])?;
    let mut rep = FmtReport {
        scanned: 0,
        changed: Vec::new(),
        lint: Vec::new(),
        skipped: Vec::new(),
    };

    for path in files {
        if is_excluded(&path, &spec.exclude) {
            continue;
        }
        let original = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                return Err(e).with_context(|| format!("read {}", path.display()));
            }
        };
        // Only touch text files; a stray binary under src/ is left alone.
        let text = match String::from_utf8(original.clone()) {
            Ok(t) => t,
            Err(_) => {
                rep.skipped.push(path);
                continue;
            }
        };
        rep.scanned += 1;

        let formatted = format_text(&text, spec);

        // Lint the *formatted* text so reported columns match what we leave on disk.
        if spec.max_line_len > 0 {
            for (i, line) in formatted.lines().enumerate() {
                let cols = line.chars().count();
                if cols > spec.max_line_len {
                    rep.lint.push((
                        path.clone(),
                        i + 1,
                        format!("line is {cols} cols (> {})", spec.max_line_len),
                    ));
                }
            }
        }

        if formatted.as_bytes() != original.as_slice() {
            rep.changed.push(path.clone());
            if !check {
                std::fs::write(&path, formatted.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
            }
        }
    }

    Ok(rep)
}

/// True if any exclude substring appears in the file's (slash-normalized) path.
fn is_excluded(path: &Path, excludes: &[String]) -> bool {
    let norm = path.to_string_lossy().replace('\\', "/");
    excludes.iter().any(|ex| !ex.is_empty() && norm.contains(ex.as_str()))
}

/// Apply the safe whitespace transforms and return the new file contents.
pub fn format_text(input: &str, spec: &FormatSpec) -> String {
    let w = spec.indent_width.max(1);

    // Normalize line endings first, then process line by line. Splitting on '\n'
    // after stripping '\r' handles CRLF and lone-CR-less files uniformly.
    let unified = input.replace("\r\n", "\n").replace('\r', "\n");

    let mut out: Vec<String> = Vec::new();
    let mut blanks = 0usize;
    for raw in unified.split('\n') {
        let expanded = expand_indent(raw, w, spec.expand_all_tabs);
        let trimmed = expanded.trim_end().to_string();
        if trimmed.is_empty() {
            blanks += 1;
            if blanks <= spec.max_blank_lines {
                out.push(String::new());
            }
        } else {
            blanks = 0;
            out.push(trimmed);
        }
    }

    // Trim blank lines at the very start and end of the file.
    while out.first().map_or(false, |l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().map_or(false, |l| l.is_empty()) {
        out.pop();
    }

    if out.is_empty() {
        String::new()
    } else {
        let mut s = out.join("\n");
        s.push('\n');
        s
    }
}

/// Expand tabs in a line. Leading tabs always become spaces (snapped to the
/// next `w` column); interior tabs are expanded only when `all` is set, so by
/// default a tab inside a `"..."`/`'...'` literal is preserved.
fn expand_indent(line: &str, w: usize, all: bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    let mut chars = line.chars().peekable();

    // Leading whitespace run.
    while let Some(&c) = chars.peek() {
        match c {
            ' ' => {
                out.push(' ');
                col += 1;
                chars.next();
            }
            '\t' => {
                let adv = w - (col % w);
                for _ in 0..adv {
                    out.push(' ');
                }
                col += adv;
                chars.next();
            }
            _ => break,
        }
    }

    // Remainder of the line.
    for c in chars {
        if all && c == '\t' {
            let adv = w - (col % w);
            for _ in 0..adv {
                out.push(' ');
            }
            col += adv;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> FormatSpec {
        FormatSpec::default()
    }

    #[test]
    fn strips_trailing_and_crlf() {
        let got = format_text("int a;   \r\nint b;\t\r\n", &spec());
        assert_eq!(got, "int a;\nint b;\n");
    }

    #[test]
    fn expands_leading_tabs_only() {
        // one leading tab -> 4 spaces; the tab inside the string stays.
        let got = format_text("\tputs(\"a\tb\");\n", &spec());
        assert_eq!(got, "    puts(\"a\tb\");\n");
    }

    #[test]
    fn collapses_blank_runs_and_edges() {
        let got = format_text("\n\n\nint a;\n\n\n\nint b;\n\n\n", &spec());
        assert_eq!(got, "int a;\n\nint b;\n");
    }

    #[test]
    fn adds_final_newline() {
        let got = format_text("int a;", &spec());
        assert_eq!(got, "int a;\n");
    }

    #[test]
    fn already_formatted_is_stable() {
        let s = "int a;\n\nint b;\n";
        assert_eq!(format_text(s, &spec()), s);
    }
}
