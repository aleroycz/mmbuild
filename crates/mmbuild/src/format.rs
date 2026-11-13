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

//! `mmbuild fmt/format`: safe C whitespace and control-spacing fixes per .clang-format subset.

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

/// Format every matching source. In `check` mode nothing is written and `changed` lists the files
/// that would change.
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
    excludes
        .iter()
        .any(|ex| !ex.is_empty() && norm.contains(ex.as_str()))
}

/// Apply the safe whitespace + spacing transforms and return the new contents.
pub fn format_text(input: &str, spec: &FormatSpec) -> String {
    let w = spec.indent_width.max(1);

    // Normalize line endings first, then process line by line. Splitting on '\n' after stripping
    // '\r' handles CRLF and lone-CR-less files uniformly.
    let unified = input.replace("\r\n", "\n").replace('\r', "\n");

    let mut out: Vec<String> = Vec::new();
    let mut blanks = 0usize;
    let mut in_block = false; // inside a /* ... */ carried across lines
    for raw in unified.split('\n') {
        let expanded = expand_indent(raw, w, spec.expand_all_tabs);
        let spaced = fix_spacing_line(&expanded, &mut in_block);
        let trimmed = spaced.trim_end().to_string();
        if trimmed.is_empty() {
            blanks += 1;
            // Collapse runs, and drop a blank right after an opening brace.
            let after_brace = out.last().is_some_and(|l| l.ends_with('{'));
            if blanks <= spec.max_blank_lines && !after_brace {
                out.push(String::new());
            }
        } else {
            blanks = 0;
            out.push(trimmed);
        }
    }

    // Trim blank lines at the very start and end of the file.
    while out.first().is_some_and(|l| l.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
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

/// Expand tabs in a line. Leading tabs always become spaces (snapped to the next `w` column);
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

/// Split line into code vs literal/comment spans.
enum Span {
    Code(String),
    Verb(String),
    Comment(String),
}

/// Split line into code vs literal/comment spans; carry in_block.
fn split_spans(line: &str, in_block: &mut bool) -> Vec<Span> {
    let ch: Vec<char> = line.chars().collect();
    let n = ch.len();
    let mut spans = Vec::new();
    let mut code = String::new();
    let mut i = 0;

    while i < n {
        if *in_block {
            let mut v = String::new();
            while i < n {
                let c = ch[i];
                v.push(c);
                i += 1;
                if c == '*' && i < n && ch[i] == '/' {
                    v.push('/');
                    i += 1;
                    *in_block = false;
                    break;
                }
            }
            spans.push(Span::Verb(v));
            continue;
        }
        let c = ch[i];
        match c {
            '"' | '\'' => {
                if !code.is_empty() {
                    spans.push(Span::Code(std::mem::take(&mut code)));
                }
                let q = c;
                let mut v = String::from(c);
                i += 1;
                while i < n {
                    let d = ch[i];
                    v.push(d);
                    i += 1;
                    if d == '\\' && i < n {
                        v.push(ch[i]);
                        i += 1;
                        continue;
                    }
                    if d == q {
                        break;
                    }
                }
                spans.push(Span::Verb(v));
            }
            '/' if i + 1 < n && ch[i + 1] == '/' => {
                if !code.is_empty() {
                    spans.push(Span::Code(std::mem::take(&mut code)));
                }
                spans.push(Span::Comment(ch[i..].iter().collect()));
                break;
            }
            '/' if i + 1 < n && ch[i + 1] == '*' => {
                if !code.is_empty() {
                    spans.push(Span::Code(std::mem::take(&mut code)));
                }
                let mut v = String::from("/*");
                i += 2;
                while i < n {
                    let d = ch[i];
                    v.push(d);
                    i += 1;
                    if d == '*' && i < n && ch[i] == '/' {
                        v.push('/');
                        i += 1;
                        break;
                    }
                }
                if !v.ends_with("*/") {
                    *in_block = true;
                }
                spans.push(Span::Comment(v));
            }
            _ => {
                code.push(c);
                i += 1;
            }
        }
    }
    if !code.is_empty() {
        spans.push(Span::Code(code));
    }
    spans
}

fn fix_spacing_line(line: &str, in_block: &mut bool) -> String {
    // Preprocessor directives: leave the code alone (macro arg lists are whitespace-sensitive);
    // whitespace cleanup already ran on the line.
    if !*in_block && line.trim_start().starts_with('#') {
        return line.to_string();
    }
    let spans = split_spans(line, in_block);
    let last = spans.len().saturating_sub(1);
    let mut out = String::new();
    let mut first_code = true;
    for (idx, sp) in spans.into_iter().enumerate() {
        match sp {
            Span::Code(s) => {
                out.push_str(&fix_code_span(&s, first_code));
                first_code = false;
            }
            Span::Verb(v) => out.push_str(&v),
            Span::Comment(c) => {
                // At least two spaces before a comment that *trails* code at the end of the line
                // (SpacesBeforeTrailingComments: 2). Wider gaps stay so AlignTrailingComments is
                // intact. A comment with more code after it (idx != last) or a full-line comment
                // keeps its original spacing.
                if idx == last && !out.trim().is_empty() {
                    let gap = out.len() - out.trim_end().len();
                    if gap < 2 {
                        for _ in gap..2 {
                            out.push(' ');
                        }
                    }
                }
                out.push_str(&c);
            }
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Apply the bracket/keyword spacing rules to a single code span (no literals or comments inside).
fn fix_code_span(s: &str, is_first: bool) -> String {
    let ch: Vec<char> = s.chars().collect();
    let n = ch.len();
    let mut out = String::with_capacity(n + 2);
    let mut i = 0;
    let mut seen = false; // emitted a non-space char on this line yet?
    while i < n {
        let c = ch[i];
        if c == ' ' {
            let mut j = i;
            while j < n && ch[j] == ' ' {
                j += 1;
            }
            let next = ch.get(j).copied();
            if is_first && !seen {
                // Leading indentation: keep verbatim.
                for _ in 0..(j - i) {
                    out.push(' ');
                }
            } else if matches!(next, Some(')') | Some(']')) {
                // SpacesInParentheses/Brackets: drop spaces before ) or ].
            } else {
                for _ in 0..(j - i) {
                    out.push(' ');
                }
            }
            i = j;
            continue;
        }
        out.push(c);
        seen = true;
        i += 1;
        // SpacesInParentheses/Brackets: drop spaces just after ( or [.
        if c == '(' || c == '[' {
            while i < n && ch[i] == ' ' {
                i += 1;
            }
        }
    }
    normalize_ctrl_paren(&out)
}

/// `if`/`for`/`while`/`switch` get exactly one space before their `(` (SpaceBeforeParens:
/// ControlStatements).
fn normalize_ctrl_paren(s: &str) -> String {
    const KW: [&str; 4] = ["if", "for", "while", "switch"];
    let ch: Vec<char> = s.chars().collect();
    let n = ch.len();
    let mut out = String::with_capacity(n + 2);
    let mut i = 0;
    while i < n {
        if is_ident(ch[i]) && (i == 0 || !is_ident(ch[i - 1])) {
            let start = i;
            while i < n && is_ident(ch[i]) {
                i += 1;
            }
            let word: String = ch[start..i].iter().collect();
            if KW.contains(&word.as_str()) {
                let mut j = i;
                while j < n && ch[j] == ' ' {
                    j += 1;
                }
                if j < n && ch[j] == '(' {
                    out.push_str(&word);
                    out.push(' ');
                    i = j;
                    continue;
                }
            }
            out.push_str(&word);
            continue;
        }
        out.push(ch[i]);
        i += 1;
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

    #[test]
    fn ctrl_keyword_gets_one_space() {
        assert_eq!(format_text("if(x) {\n", &spec()), "if (x) {\n");
        assert_eq!(format_text("for(;;) {\n", &spec()), "for (;;) {\n");
        assert_eq!(format_text("while  (x) {\n", &spec()), "while (x) {\n");
    }

    #[test]
    fn call_paren_untouched() {
        // Function calls keep no space (ControlStatements only); a function whose name merely
        // starts with a keyword is left alone.
        assert_eq!(format_text("notify(x);\n", &spec()), "notify(x);\n");
        assert_eq!(
            format_text("switch_mode(x);\n", &spec()),
            "switch_mode(x);\n"
        );
    }

    #[test]
    fn inner_paren_and_bracket_spaces_stripped() {
        assert_eq!(format_text("foo( a, b );\n", &spec()), "foo(a, b);\n");
        assert_eq!(format_text("a[ i ] = 0;\n", &spec()), "a[i] = 0;\n");
        assert_eq!(format_text("foo(  );\n", &spec()), "foo();\n");
    }

    #[test]
    fn literals_and_casts_are_safe() {
        // Parens/tabs inside a string must not be touched.
        assert_eq!(
            format_text("puts(\"a )b\");\n", &spec()),
            "puts(\"a )b\");\n"
        );
        // C-style cast keeps its space (SpaceAfterCStyleCast).
        assert_eq!(format_text("x = (int) y;\n", &spec()), "x = (int) y;\n");
    }

    #[test]
    fn trailing_comment_two_spaces() {
        assert_eq!(format_text("x = 1; // c\n", &spec()), "x = 1;  // c\n");
        // A full-line comment keeps its indentation.
        assert_eq!(format_text("    // note\n", &spec()), "    // note\n");
    }

    #[test]
    fn midline_block_comment_spacing_untouched() {
        // A block comment with code after it is not a trailing comment.
        assert_eq!(
            format_text("f(int x /*0..1*/, int y);\n", &spec()),
            "f(int x /*0..1*/, int y);\n"
        );
        assert_eq!(
            format_text("if (w < 0) { /* ignore */ }\n", &spec()),
            "if (w < 0) { /* ignore */ }\n"
        );
    }

    #[test]
    fn aligned_trailing_comments_preserved() {
        // AlignTrailingComments: 2 is a minimum, not a target - wide gaps stay.
        let s = "int a;     /* x */\nint bb;    /* y */\n";
        assert_eq!(format_text(s, &spec()), s);
    }

    #[test]
    fn preprocessor_macro_args_untouched() {
        assert_eq!(
            format_text("#define M(x) ((x) + 1)\n", &spec()),
            "#define M(x) ((x) + 1)\n"
        );
    }

    #[test]
    fn blank_line_after_brace_removed() {
        assert_eq!(
            format_text("void f(void) {\n\n    g();\n}\n", &spec()),
            "void f(void) {\n    g();\n}\n"
        );
    }

    #[test]
    fn block_comment_across_lines_is_safe() {
        let s = "a;  /* one\n   two */ b;\n";
        // No spacing change inside the comment; code around it still cleaned.
        assert_eq!(format_text(s, &spec()), "a;  /* one\n   two */ b;\n");
    }
}
