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

//! semver/build number in version.h and git stamp into version_gen.h.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// Parsed semantic version + build number from `src/version.h`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub build: u64,
}

impl VersionInfo {
    pub fn semver(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
    /// e.g. "1.0.0+42 (a1b2c3d)".
    pub fn full(&self, short: &str, dirty: bool) -> String {
        let suffix = if dirty { "-dirty" } else { "" };
        format!("{}+{} ({}{})", self.semver(), self.build, short, suffix)
    }
}

/// Git/environment provenance for the generated header.
#[derive(Debug, Clone)]
pub struct GitInfo {
    pub short: String,
    pub long: String,
    pub dirty: bool,
    pub built_by: String,
    pub has_git: bool,
}

pub fn version_h(root: &Path) -> PathBuf {
    root.join("src").join("version.h")
}
pub fn version_gen_h(root: &Path) -> PathBuf {
    root.join("src").join("version_gen.h")
}

/// Read and parse `src/version.h`.
pub fn read(root: &Path) -> Result<VersionInfo> {
    let path = version_h(root);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse(&text)
}

fn parse(text: &str) -> Result<VersionInfo> {
    let major = define_u32(text, "MM_VERSION_MAJOR")?;
    let minor = define_u32(text, "MM_VERSION_MINOR")?;
    let patch = define_u32(text, "MM_VERSION_PATCH")?;
    let build = define_u64(text, "MM_BUILD_NUMBER")?;
    Ok(VersionInfo {
        major,
        minor,
        patch,
        build,
    })
}

/// Extract the integer value of `#define <name> <int>`.
fn define_int(text: &str, name: &str) -> Result<u64> {
    for line in text.lines() {
        let t = line.trim();
        let rest = match t.strip_prefix("#define") {
            Some(r) => r.trim_start(),
            None => continue,
        };
        let rest = match rest.strip_prefix(name) {
            Some(r) => r,
            None => continue,
        };
        // Ensure a full-token match (next char is whitespace), not a prefix.
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let val = rest.trim();
        // Allow a trailing comment.
        let val = val.split("//").next().unwrap_or(val);
        let val = val.split("/*").next().unwrap_or(val).trim();
        return val
            .parse::<u64>()
            .with_context(|| format!("parse #define {name} value {val:?}"));
    }
    bail!("missing #define {name}")
}

fn define_u32(text: &str, name: &str) -> Result<u32> {
    let v = define_int(text, name)?;
    u32::try_from(v).with_context(|| format!("{name} value {v} does not fit u32"))
}

fn define_u64(text: &str, name: &str) -> Result<u64> {
    define_int(text, name)
}

/// Replace `#define <name> <old>` with a new integer, preserving other lines.
fn replace_define(text: &str, name: &str, value: u64) -> Result<String> {
    let mut out = String::new();
    let mut found = false;
    for line in text.lines() {
        let t = line.trim();
        let mut replaced = false;
        if t.starts_with("#define") {
            let rest = t.strip_prefix("#define").unwrap().trim_start();
            if let Some(after) = rest.strip_prefix(name) {
                if after.starts_with(char::is_whitespace) {
                    out.push_str(&format!("#define {name} {value}"));
                    replaced = true;
                    found = true;
                }
            }
        }
        if !replaced {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !found {
        bail!("missing #define {name}");
    }
    Ok(out)
}

/// Increment `MM_BUILD_NUMBER` in `src/version.h`.
pub fn bump_build(root: &Path) -> Result<VersionInfo> {
    let path = version_h(root);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut info = parse(&text)?;
    info.build = info.build.saturating_add(1);
    let next = replace_define(&text, "MM_BUILD_NUMBER", info.build)?;
    std::fs::write(&path, next).with_context(|| format!("write {}", path.display()))?;
    Ok(info)
}

fn committed_version(root: &Path) -> Option<VersionInfo> {
    let out = Command::new("git")
        .args(["show", "HEAD:src/version.h"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse(&text).ok()
}

/// Collect git identity for provenance stamping.
pub fn git_info(root: &Path) -> GitInfo {
    let short =
        git_one(root, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let long = git_one(root, &["rev-parse", "HEAD"]).unwrap_or_else(|| short.clone());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    let has_git = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let built_by = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    GitInfo {
        short,
        long,
        dirty,
        built_by,
        has_git,
    }
}

fn git_one(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Write `src/version_gen.h` with git and build metadata.
pub fn stamp(root: &Path, info: &VersionInfo, git: &GitInfo) -> Result<()> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let date = utc_stamp(secs);
    let dirty = if git.dirty { 1 } else { 0 };
    let tool = "mmbuild 0.1.0 (zig cc)";
    let body = format!(
        "/* Generated by mmbuild; do not edit or commit. Provenance for {}+{}. */\n\
         #ifndef MM_VERSION_GEN_H\n\
         #define MM_VERSION_GEN_H\n\
         #define MM_GIT_COMMIT_SHORT {short:?}\n\
         #define MM_GIT_COMMIT_LONG {long:?}\n\
         #define MM_GIT_DIRTY {dirty}\n\
         #define MM_BUILT_BY {built_by:?}\n\
         #define MM_BUILD_TOOL {tool:?}\n\
         #define MM_BUILD_DATE {date:?}\n\
         #endif /* MM_VERSION_GEN_H */\n",
        info.semver(),
        info.build,
        short = git.short,
        long = git.long,
        dirty = dirty,
        built_by = git.built_by,
        tool = tool,
        date = date,
    );
    let path = version_gen_h(root);
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Bump the build number and stamp provenance. Called at the start of a build.
pub fn prepare(root: &Path) -> Result<(VersionInfo, GitInfo)> {
    let info = bump_build(root)?;
    let gi = git_info(root);
    stamp(root, &info, &gi)?;
    Ok((info, gi))
}

/// Assert the working-tree semantic version differs from `HEAD`.
pub fn assert_bumped(root: &Path) -> Result<()> {
    let working = read(root)?;
    match committed_version(root) {
        None => {
            println!("version: no committed version.h to compare against (first commit) - ok");
            Ok(())
        }
        Some(head) => {
            if (working.major, working.minor, working.patch) == (head.major, head.minor, head.patch)
            {
                bail!(
                    "version not bumped: src/version.h is still {} (same as HEAD).\n\
                     Edit MM_VERSION_{{MAJOR,MINOR,PATCH}} in src/version.h, or set \
                     MM_SKIP_VERSION_CHECK=1 to bypass for this commit.",
                    working.semver()
                );
            }
            println!("version: {} -> {} - ok", head.semver(), working.semver());
            Ok(())
        }
    }
}

/// Format a Unix timestamp (UTC seconds) as "YYYY-MM-DD HH:MM:SSZ".
fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
#define MM_VERSION_MAJOR 2\n\
#define MM_VERSION_MINOR 5\n\
#define MM_VERSION_PATCH 9\n\
#define MM_BUILD_NUMBER 41\n";

    #[test]
    fn parses_fields() {
        let v = parse(SAMPLE).unwrap();
        assert_eq!(
            v,
            VersionInfo {
                major: 2,
                minor: 5,
                patch: 9,
                build: 41
            }
        );
        assert_eq!(v.semver(), "2.5.9");
        assert_eq!(v.full("abc123", false), "2.5.9+41 (abc123)");
        assert_eq!(v.full("abc123", true), "2.5.9+41 (abc123-dirty)");
    }

    #[test]
    fn replaces_build_number_only() {
        let out = replace_define(SAMPLE, "MM_BUILD_NUMBER", 42).unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v.build, 42);
        assert_eq!(v.major, 2); // untouched
        assert!(out.contains("#define MM_BUILD_NUMBER 42"));
    }

    #[test]
    fn ignores_prefix_collisions() {
        let text = "#define MM_BUILD_NUMBER_EXTRA 7\n#define MM_BUILD_NUMBER 3\n";
        assert_eq!(define_u64(text, "MM_BUILD_NUMBER").unwrap(), 3);
    }

    #[test]
    fn utc_epoch_zero() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00Z");
        assert_eq!(utc_stamp(1_700_000_000), "2023-11-14 22:13:20Z");
    }
}
