// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! Discover, build, and install sub-projects from nested `[subproject]` build.toml.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;
use sha2::{Digest, Sha256};

use crate::config::{
    discover_subprojects, ExternalSpec, InstallSpec, Settings, SubKind, SubprojectConfig,
};
use crate::toolchain::compile_c;

/// Build every discovered sub-project (dependency order), optionally filtered to a single `name`.
pub fn build_all(
    root: &Path,
    settings: &Settings,
    log: &Logger,
    only: Option<&str>,
    include_manual: bool,
    model: &str,
) -> Result<Vec<String>> {
    let found = discover_subprojects(root)?;
    if found.is_empty() {
        log.info("no sub-projects found");
        return Ok(Vec::new());
    }

    let mut built = Vec::new();
    for (dir, cfg) in &found {
        let name = cfg.subproject.name.as_str();
        if let Some(want) = only {
            if name != want {
                continue;
            }
        } else if cfg.subproject.manual && !include_manual {
            log.detail(&format!("skip sub-project '{name}' (manual)"));
            continue;
        }
        log.info(&format!(
            "sub-project '{name}' [{}]{}",
            kind_str(cfg.subproject.kind),
            if cfg.subproject.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", cfg.subproject.description)
            }
        ));
        build_one(root, dir, cfg, settings, log, model)
            .with_context(|| format!("sub-project '{name}'"))?;
        built.push(name.to_string());
    }

    if let Some(want) = only {
        if built.is_empty() {
            bail!("no sub-project named '{want}' (have: {})", names(&found));
        }
    }
    Ok(built)
}

fn names(found: &[(PathBuf, SubprojectConfig)]) -> String {
    found
        .iter()
        .map(|(_, c)| c.subproject.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn kind_str(k: SubKind) -> &'static str {
    match k {
        SubKind::Cbinary => "cbinary",
        SubKind::External => "external",
        SubKind::Copy => "copy",
    }
}

fn build_one(
    root: &Path,
    _dir: &Path,
    cfg: &SubprojectConfig,
    settings: &Settings,
    log: &Logger,
    model: &str,
) -> Result<()> {
    match cfg.subproject.kind {
        SubKind::Cbinary => {
            let mut spec = cfg
                .build
                .as_ref()
                .context("[subproject] kind=cbinary requires a [build] section")?
                .clone();
            // device_compat profile is authoritative for the compile target here too, so a
            // sub-project can't drift from the main binary's arch/-mcpu
            for note in
                crate::compat::reconcile_target(root, model, &mut spec.target, &mut spec.mcpu)?
            {
                log.info(&format!("[{}] {}", cfg.subproject.name, note));
            }
            compile_c(root, &spec, settings, log)?; // stamp written; up-to-date skips zig
        }
        SubKind::External => {
            let ext = cfg
                .external
                .as_ref()
                .context("[subproject] kind=external requires an [external] section")?;
            prepare_external(root, ext, log)?;
        }
        SubKind::Copy => {
            if cfg.install.is_empty() {
                log.warn("kind=copy sub-project has no [[install]] entries");
            }
        }
    }
    run_installs(root, &cfg.install, log)?;
    Ok(())
}

/// Apply [[install]] from->to copies under root.
fn run_installs(root: &Path, installs: &[InstallSpec], log: &Logger) -> Result<()> {
    for ins in installs {
        let from = root.join(&ins.from);
        let to = root.join(&ins.to);
        if !from.exists() {
            bail!("install source missing: {}", from.display());
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::copy(&from, &to)
            .with_context(|| format!("install {} -> {}", from.display(), to.display()))?;
        set_mode(&to, &ins.mode);
        log.detail(&format!("installed {} -> {}", ins.from, ins.to));
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: &str) {
    if mode.is_empty() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = u32::from_str_radix(mode.trim_start_matches("0o").trim_start_matches('0'), 8) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(m));
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: &str) {
    // Windows has no POSIX mode bits; the mode is applied when the file lands in the ext4 image
    // (the image writer sets 0755 on /system/bin binaries).
}

/// Fetch/build external dep; skip if dir/artifacts exist.
fn prepare_external(root: &Path, ext: &ExternalSpec, log: &Logger) -> Result<()> {
    let dir = root.join(&ext.dir);

    // 1. Fetch + extract the source tree if it is not already present.
    if dir.exists() {
        log.detail(&format!("external dir present: {}", ext.dir));
    } else {
        if ext.url.is_empty() {
            bail!(
                "external dir {} is missing and no url is set to fetch it",
                ext.dir
            );
        }
        let archive = archive_path(root, ext);
        if !archive.exists() {
            download(&ext.url, &archive, log)?;
        }
        if !ext.sha256.is_empty() {
            verify_sha256(&archive, &ext.sha256, log)?;
        } else {
            log.warn("external archive has no sha256 — integrity not verified");
        }
        let parent = dir.parent().unwrap_or(root);
        extract_tar_gz(&archive, parent, log)
            .with_context(|| format!("extract {}", archive.display()))?;
        if !dir.exists() {
            bail!(
                "archive did not produce expected dir {} (check [external].dir)",
                ext.dir
            );
        }
    }

    // 2. Build the dep if it declares a command and its artifacts are missing.
    let artifacts_present =
        !ext.artifacts.is_empty() && ext.artifacts.iter().all(|a| dir.join(a).exists());
    if ext.build_command.is_empty() {
        if !ext.artifacts.is_empty() && !artifacts_present {
            log.warn("external artifacts missing and no build_command set");
        }
        return Ok(());
    }
    if artifacts_present {
        log.detail("external artifacts present; skipping build");
        return Ok(());
    }

    let prog = &ext.build_command[0];
    log.info(&format!(
        "building external: {}",
        ext.build_command.join(" ")
    ));
    let mut cmd = Command::new(prog);
    cmd.args(&ext.build_command[1..]);
    cmd.current_dir(&dir);
    let out = cmd
        .output()
        .with_context(|| format!("spawn `{}` in {}", prog, dir.display()))?;
    if !out.stdout.is_empty() {
        log.debug(&String::from_utf8_lossy(&out.stdout));
    }
    if !out.status.success() {
        log.warn(&String::from_utf8_lossy(&out.stderr));
        bail!("external build failed ({})", out.status);
    }
    for a in &ext.artifacts {
        if !dir.join(a).exists() {
            bail!("external build did not produce artifact {a}");
        }
    }
    Ok(())
}

fn archive_path(root: &Path, ext: &ExternalSpec) -> PathBuf {
    if !ext.archive.is_empty() {
        return root.join(&ext.archive);
    }
    let base = ext
        .url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("external.tar.gz");
    root.join("external").join(base)
}

fn download(url: &str, dest: &Path, log: &Logger) -> Result<()> {
    log.info(&format!("downloading {url}"));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut buf).context("read response body")?;
    std::fs::write(dest, &buf).with_context(|| format!("write {}", dest.display()))?;
    log.detail(&format!(
        "saved {} ({})",
        dest.display(),
        mmcore::human_bytes(buf.len() as u64)
    ));
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str, log: &Logger) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&data);
    let got = hex_lower(&h.finalize());
    if !got.eq_ignore_ascii_case(expected.trim()) {
        bail!(
            "sha256 mismatch for {}\n  expected {}\n  got      {}",
            path.display(),
            expected.trim(),
            got
        );
    }
    log.detail("sha256 verified");
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Extract a gzip-compressed POSIX tar (ustar / GNU) into `dest_dir`.
fn extract_tar_gz(archive: &Path, dest_dir: &Path, log: &Logger) -> Result<()> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    std::fs::create_dir_all(dest_dir).ok();
    let f = std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut gz = GzDecoder::new(f);
    let mut tar = Vec::new();
    gz.read_to_end(&mut tar).context("gunzip archive")?;

    let mut off = 0usize;
    let mut count = 0usize;
    // GNU long-name support: a 'L' typeflag record carries the next entry's name.
    let mut long_name: Option<String> = None;
    while off + 512 <= tar.len() {
        let hdr = &tar[off..off + 512];
        // Two consecutive zero blocks mark end-of-archive.
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let size = parse_octal(&hdr[124..136]).unwrap_or(0) as usize;
        let typeflag = hdr[156];
        let name = match long_name.take() {
            Some(n) => n,
            None => read_tar_name(hdr),
        };
        off += 512;
        let data = &tar[off..(off + size).min(tar.len())];

        match typeflag {
            b'L' => {
                // GNU long name: the data block is the real name for the next hdr.
                long_name = Some(
                    String::from_utf8_lossy(data)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'5' => {
                // Directory.
                if !name.is_empty() {
                    std::fs::create_dir_all(dest_dir.join(sanitize(&name))).ok();
                }
            }
            b'0' | 0 => {
                // Regular file.
                let path = dest_dir.join(sanitize(&name));
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&path, data).with_context(|| format!("write {}", path.display()))?;
                count += 1;
            }
            // Symlinks/hardlinks/char/block/fifo: skipped (not needed for our deps).
            _ => {}
        }
        // Advance past the data, padded to the next 512-byte boundary.
        off += (size + 511) & !511;
    }
    log.detail(&format!(
        "extracted {count} file(s) to {}",
        dest_dir.display()
    ));
    Ok(())
}

fn read_tar_name(hdr: &[u8]) -> String {
    let name = cstr(&hdr[0..100]);
    // ustar prefix field (offset 345, len 155) is prepended when present.
    let prefix = cstr(&hdr[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

fn parse_octal(b: &[u8]) -> Option<u64> {
    let s: String = b
        .iter()
        .map(|&c| c as char)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if s.is_empty() {
        Some(0)
    } else {
        u64::from_str_radix(&s, 8).ok()
    }
}

/// Reject absolute paths and `..` traversal so a malicious archive cannot write outside the
/// destination directory.
fn sanitize(name: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in name.split(['/', '\\']) {
        match comp {
            "" | "." | ".." => continue,
            c => out.push(c),
        }
    }
    out
}
