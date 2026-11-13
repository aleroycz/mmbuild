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

//! Locate Zig and cross-compile C via `zig cc`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;
use walkdir::WalkDir;

use crate::config::{BuildConfig, Settings, SourceSpec};
use crate::fingerprint;

/// Find the Zig compiler: explicit setting, bundled `bin/zig/zig(.exe)`, or PATH.
pub fn find_zig(root: &Path, settings: &Settings) -> Result<PathBuf> {
    let exe = if cfg!(windows) { "zig.exe" } else { "zig" };
    let bundled = root.join("bin").join("zig").join(exe);

    if !settings.toolchain.zig.is_empty() {
        let p = resolve(root, &settings.toolchain.zig);
        if p.exists() {
            return Ok(p);
        }
        bail!(
            "configured zig not found at {}\n  \
             Fix [toolchain].zig in settings.toml, or clear it to auto-discover \
             bundled bin/zig/{} or PATH.\n  Hint: run `mmbuild doctor`.",
            p.display(),
            exe
        );
    }
    if bundled.exists() {
        return Ok(bundled);
    }
    match which(exe) {
        Ok(p) => Ok(p),
        Err(_) => bail!(
            "zig not found (looked for [toolchain].zig, {}, and PATH)\n  \
             Install Zig, place it at bin/zig/{}, or set [toolchain].zig in \
             settings.toml.\n  Hint: run `mmbuild doctor`.",
            bundled.display(),
            exe
        ),
    }
}

fn which(exe: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(exe);
        if cand.exists() {
            return Ok(cand);
        }
    }
    bail!("{exe} not found on PATH")
}

fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}

/// Expand include globs (supports `dir/**/*.ext` and `dir/*.ext` and exact files) into a sorted,
/// de-duplicated file list.
pub fn collect_sources(
    root: &Path,
    patterns: &[String],
    excludes: &[String],
) -> Result<Vec<PathBuf>> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for pat in patterns {
        if let Some(idx) = pat.find("**") {
            let base = &pat[..idx];
            let after = pat[idx + 2..].trim_start_matches('/');
            let ext = after.rsplit('.').next().map(|e| e.to_string());
            let base_dir = resolve(root, base.trim_end_matches('/'));
            for e in WalkDir::new(&base_dir).into_iter().filter_map(|e| e.ok()) {
                if e.file_type().is_file() {
                    let keep = match &ext {
                        Some(ext) if after.starts_with("*.") => e
                            .path()
                            .extension()
                            .map(|x| x == ext.as_str())
                            .unwrap_or(false),
                        _ => true,
                    };
                    if keep {
                        set.insert(e.path().to_path_buf());
                    }
                }
            }
        } else if let Some(stripped) = pat.strip_suffix("/*") {
            let dir = resolve(root, stripped);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    if e.path().is_file() {
                        set.insert(e.path());
                    }
                }
            }
        } else if pat.contains("/*.") {
            // dir/*.ext  (one level)
            let (dir, star) = pat.rsplit_once('/').unwrap();
            let ext = star.trim_start_matches("*.");
            let dir = resolve(root, dir);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    if e.path().extension().map(|x| x == ext).unwrap_or(false) {
                        set.insert(e.path());
                    }
                }
            }
        } else {
            let p = resolve(root, pat);
            if p.is_file() {
                set.insert(p);
            }
        }
    }
    // Drop excluded files (matched by canonical path, so relative spellings line up).
    for ex in excludes {
        let p = resolve(root, ex);
        set.remove(&p);
        if let Ok(cp) = std::fs::canonicalize(&p) {
            set.retain(|s| std::fs::canonicalize(s).map(|c| c != cp).unwrap_or(true));
        }
    }
    Ok(set.into_iter().collect())
}

/// Cross-compile the device binary with `zig cc`.
pub fn compile_device(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    log: &Logger,
) -> Result<CompileResult> {
    compile_c(root, &cfg.source, settings, log)
}

/// Outcome of a `zig cc` build (or an up-to-date skip).
pub struct CompileResult {
    #[allow(dead_code)] // available to callers; pipeline currently uses `up_to_date` only
    pub output: PathBuf,
    pub up_to_date: bool,
}

/// Compile a C binary described by a [`SourceSpec`] with `zig cc`.
pub fn compile_c(
    root: &Path,
    spec: &SourceSpec,
    settings: &Settings,
    log: &Logger,
) -> Result<CompileResult> {
    let zig = find_zig(root, settings)?;
    let sources = collect_sources(root, &spec.include, &spec.exclude)?;
    if sources.is_empty() {
        bail!(
            "no sources matched {:?} (after excludes {:?})\n  \
             Check [source].include / [build].include globs relative to the project root.",
            spec.include,
            spec.exclude
        );
    }
    log.info(&format!("zig: {}", zig.display()));
    log.info(&format!("compiling {} source files", sources.len()));

    let output = resolve(root, &spec.output);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Pragmatic incremental: skip zig cc when sources/headers/flags/zig match the stamp written
    // next to the output on the last successful link.
    let digest = fingerprint::compile_digest(
        root,
        &sources,
        &spec.include_dirs,
        &spec.target,
        &spec.mcpu,
        &spec.cflags,
        &spec.third_party,
        &output,
        &zig,
    )?;
    let stamp = fingerprint::compile_stamp_path(&output);
    if output.is_file() && fingerprint::read_stamp(&stamp).as_deref() == Some(digest.as_str()) {
        let size = std::fs::metadata(&output)?.len();
        log.info(&format!(
            "up-to-date {} ({})",
            output.display(),
            mmcore::human_bytes(size)
        ));
        return Ok(CompileResult {
            output,
            up_to_date: true,
        });
    }

    // Split vendored/third-party TUs out of the first-party set. We never edit upstream sources to
    // satisfy our warning flags, so those TUs are compiled to objects with warnings off and linked
    // in, while first-party code is held to the full strict `-Werror` set below.
    let (vendored, first_party): (Vec<_>, Vec<_>) = sources.iter().cloned().partition(|s| {
        let p = s.to_string_lossy();
        spec.third_party.iter().any(|m| p.contains(m.as_str()))
    });

    let mut vendored_objs: Vec<PathBuf> = Vec::new();
    if !vendored.is_empty() {
        let obj_dir = output
            .parent()
            .map(|p| p.join("obj"))
            .unwrap_or_else(|| PathBuf::from("obj"));
        std::fs::create_dir_all(&obj_dir)
            .with_context(|| format!("create vendored object dir {}", obj_dir.display()))?;
        log.info(&format!(
            "compiling {} vendored TU(s) with warnings disabled",
            vendored.len()
        ));
        for src in &vendored {
            let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("vendor");
            let obj = obj_dir.join(format!("{stem}.o"));
            let mut vc = Command::new(&zig);
            vc.arg("cc")
                .arg("-target")
                .arg(&spec.target)
                .arg(format!("-mcpu={}", spec.mcpu));
            // Keep the non-warning build flags (-static/-O3/etc.), drop every -W/-Werror flag, and
            // silence the TU entirely with -w.
            for f in &spec.cflags {
                if f == "-lm" || f.starts_with("-W") {
                    continue;
                }
                vc.arg(f);
            }
            vc.arg("-w");
            for inc in &spec.include_dirs {
                vc.arg("-I").arg(resolve(root, inc));
            }
            vc.arg("-c").arg(src).arg("-o").arg(&obj);
            vc.current_dir(root);
            let o = vc.output().context("spawn zig cc (vendored)")?;
            if !o.status.success() {
                log.warn(&String::from_utf8_lossy(&o.stderr));
                bail!("zig cc failed on vendored {} ({})", src.display(), o.status);
            }
            vendored_objs.push(obj);
        }
    }

    let mut cmd = Command::new(&zig);
    cmd.arg("cc")
        .arg("-target")
        .arg(&spec.target)
        .arg(format!("-mcpu={}", spec.mcpu));
    for f in &spec.cflags {
        cmd.arg(f);
    }
    for inc in &spec.include_dirs {
        cmd.arg("-I").arg(resolve(root, inc));
    }
    cmd.arg("-o").arg(&output);
    for s in &first_party {
        cmd.arg(s);
    }
    for o in &vendored_objs {
        cmd.arg(o);
    }
    cmd.current_dir(root);

    log.detail("running zig cc");
    let out = cmd.output().context("spawn zig cc")?;
    if !out.stdout.is_empty() {
        log.debug(&String::from_utf8_lossy(&out.stdout));
    }
    if !out.stderr.is_empty() {
        // zig warnings/errors
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            if line.contains("error") {
                log.warn(line);
            } else {
                log.debug(line);
            }
        }
    }
    if !out.status.success() {
        bail!("zig cc failed ({})", out.status);
    }
    fingerprint::write_stamp(&stamp, &digest)?;
    let size = std::fs::metadata(&output)?.len();
    log.info(&format!(
        "built {} ({})",
        output.display(),
        mmcore::human_bytes(size)
    ));
    Ok(CompileResult {
        output,
        up_to_date: false,
    })
}
