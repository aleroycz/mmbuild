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

//! SHA-256 input fingerprints to skip unchanged firmware/compile steps.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

/// Hex SHA-256 of the concatenated fingerprint lines.
pub fn digest_lines(lines: &[String]) -> String {
    let mut h = Sha256::new();
    for line in lines {
        h.update(line.as_bytes());
        h.update([0u8]);
    }
    hex_lower(h.finalize())
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let b = bytes.as_ref();
    let mut out = String::with_capacity(b.len() * 2);
    for &x in b {
        out.push(HEX[(x >> 4) as usize] as char);
        out.push(HEX[(x & 0xf) as usize] as char);
    }
    out
}

/// One line describing a file: `path\0len\0mtime_secs` (mtime 0 if unavailable).
pub fn file_token(path: &Path) -> Result<String> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(format!("{}\0{}\0{}", path.display(), meta.len(), mtime))
}

/// Best-effort token when the file may be missing (returns a sentinel).
pub fn file_token_or_missing(path: &Path) -> String {
    match file_token(path) {
        Ok(s) => s,
        Err(_) => format!("{}\0missing", path.display()),
    }
}

pub fn read_stamp(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_stamp(path: &Path, digest: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let mut f = fs::File::create(path).with_context(|| format!("write {}", path.display()))?;
    f.write_all(digest.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Cache root for pristine firmware supers: `{cache}/firmware/<digest>/`.
pub fn firmware_cache_dir(cache_root: &Path, digest: &str) -> PathBuf {
    cache_root.join("firmware").join(digest)
}

pub fn pristine_super_path(cache_root: &Path, digest: &str) -> PathBuf {
    firmware_cache_dir(cache_root, digest).join("super.raw")
}

/// If a pristine cached super exists for `digest`, return its path.
pub fn lookup_pristine(cache_root: &Path, digest: &str) -> Option<PathBuf> {
    let p = pristine_super_path(cache_root, digest);
    match fs::metadata(&p) {
        Ok(m) if m.is_file() && m.len() > 0 => Some(p),
        _ => None,
    }
}

/// Copy `raw` into the fingerprint cache (overwrite).
pub fn store_pristine(cache_root: &Path, digest: &str, raw: &Path) -> Result<PathBuf> {
    let dest = pristine_super_path(cache_root, digest);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Same path (unpack already wrote into the cache dir) - nothing to do.
    if paths_same(raw, &dest) {
        return Ok(dest);
    }
    fs::copy(raw, &dest)
        .with_context(|| format!("cache pristine {} → {}", raw.display(), dest.display()))?;
    write_stamp(
        &firmware_cache_dir(cache_root, digest).join("source.fp"),
        digest,
    )?;
    Ok(dest)
}

fn paths_same(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Fingerprint a local firmware artifact (path + size + mtime).
pub fn local_firmware_digest(ap: &Path) -> Result<String> {
    Ok(digest_lines(&[format!("local\0{}", file_token(ap)?)]))
}

/// Fingerprint a FUS (or similar) download identity.
pub fn remote_firmware_digest(kind: &str, id_parts: &[&str]) -> String {
    let mut lines = vec![format!("remote\0{kind}")];
    for p in id_parts {
        lines.push((*p).to_string());
    }
    digest_lines(&lines)
}

/// Fingerprint for `zig cc` inputs: sources, headers under include dirs, flags, zig.
#[allow(clippy::too_many_arguments)]
pub fn compile_digest(
    root: &Path,
    sources: &[PathBuf],
    include_dirs: &[String],
    target: &str,
    mcpu: &str,
    cflags: &[String],
    third_party: &[String],
    output: &Path,
    zig: &Path,
) -> Result<String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("target\0{target}"));
    lines.push(format!("mcpu\0{mcpu}"));
    lines.push(format!("output\0{}", output.display()));
    for f in cflags {
        lines.push(format!("cflag\0{f}"));
    }
    for t in third_party {
        lines.push(format!("third_party\0{t}"));
    }
    lines.push(file_token_or_missing(zig));

    for s in sources {
        let label = s
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| s.display().to_string());
        let tok = file_token(s)?;
        // Re-label with a stable root-relative path so absolute roots don't churn.
        let rest = tok.split_once('\0').map(|(_, r)| r).unwrap_or("");
        lines.push(format!("src\0{label}\0{rest}"));
    }

    // Pragmatic header coverage: every *.h under configured include dirs.
    let mut headers: Vec<PathBuf> = Vec::new();
    for inc in include_dirs {
        let dir = {
            let p = PathBuf::from(inc);
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        };
        if !dir.is_dir() {
            continue;
        }
        for e in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file()
                && e.path()
                    .extension()
                    .map(|x| x == "h" || x == "inc")
                    .unwrap_or(false)
            {
                headers.push(e.path().to_path_buf());
            }
        }
    }
    headers.sort();
    headers.dedup();
    for h in headers {
        let label = h
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| h.display().to_string());
        let tok = file_token(&h)?;
        let rest = tok.split_once('\0').map(|(_, r)| r).unwrap_or("");
        lines.push(format!("hdr\0{label}\0{rest}"));
    }

    lines.sort();
    Ok(digest_lines(&lines))
}

/// Stamp path next to a compile output (`foo` -> `foo.stamp`).
pub fn compile_stamp_path(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_owned();
    s.push(".stamp");
    PathBuf::from(s)
}
