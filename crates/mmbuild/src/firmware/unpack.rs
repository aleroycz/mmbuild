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

//! Turn an acquired firmware artifact into a raw `super` image, recursively
//! descending through the Samsung packaging: `.zip` → `AP_*.tar(.md5)` →
//! `super.img.lz4` → sparse `super.img` → raw. Each layer is detected by magic,
//! so a local file at any level (even a bare raw super) is handled.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;
use superimg::{lz4frame, sparse};

const LP_GEOMETRY_MAGIC: u32 = 0x616c_4467;

#[derive(Debug, PartialEq)]
enum Kind {
    Lz4,
    Sparse,
    Zip,
    Tar,
    RawSuper,
    Unknown,
}

fn detect(path: &Path) -> Result<Kind> {
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 8];
    let n = f.read(&mut head)?;
    if n >= 4 && head[..4] == [0x04, 0x22, 0x4D, 0x18] {
        return Ok(Kind::Lz4);
    }
    if n >= 4 && head[..4] == [0x3A, 0xFF, 0x26, 0xED] {
        return Ok(Kind::Sparse);
    }
    if n >= 4 && head[..4] == [0x50, 0x4B, 0x03, 0x04] {
        return Ok(Kind::Zip);
    }
    // ustar magic at offset 257.
    let mut ustar = [0u8; 5];
    if f.seek(SeekFrom::Start(257)).is_ok() && f.read_exact(&mut ustar).is_ok() && &ustar == b"ustar"
    {
        return Ok(Kind::Tar);
    }
    // LP geometry at offset 4096 => already a raw super.
    let mut geo = [0u8; 4];
    if f.seek(SeekFrom::Start(4096)).is_ok()
        && f.read_exact(&mut geo).is_ok()
        && u32::from_le_bytes(geo) == LP_GEOMETRY_MAGIC
    {
        return Ok(Kind::RawSuper);
    }
    Ok(Kind::Unknown)
}

fn is_super_member(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    base == "super.img.lz4" || base == "super.img"
}
fn is_nested_archive(name: &str) -> bool {
    let b = name.to_ascii_lowercase();
    (b.starts_with("ap_") || b.contains("/ap_"))
        && (b.ends_with(".tar.md5") || b.ends_with(".tar"))
        || b.ends_with(".zip")
}

/// Produce a raw super image from any firmware artifact.
pub fn to_raw_super(input: &Path, workdir: &Path, log: &Logger) -> Result<PathBuf> {
    std::fs::create_dir_all(workdir)?;
    match detect(input)? {
        Kind::RawSuper => {
            log.info(&format!("raw super detected: {}", input.display()));
            Ok(input.to_path_buf())
        }
        Kind::Sparse => {
            let out = workdir.join("super.raw");
            log.detail("expanding sparse super → raw");
            sparse::sparse_to_raw(input, &out, None)?;
            Ok(out)
        }
        Kind::Lz4 => {
            let dec = workdir.join("super.img");
            log.detail("lz4 decompress");
            lz4frame::decompress_file(input, &dec, None)?;
            to_raw_super(&dec, workdir, log)
        }
        Kind::Tar => extract_from_tar(input, workdir, log),
        Kind::Zip => extract_from_zip(input, workdir, log),
        Kind::Unknown => bail!(
            "unrecognized firmware artifact: {} (expected zip / AP tar / super.img.lz4 / super.img / raw super)",
            input.display()
        ),
    }
}

fn extract_from_tar(input: &Path, workdir: &Path, log: &Logger) -> Result<PathBuf> {
    let mut f = std::fs::File::open(input)?;
    let total = f.metadata()?.len();
    let mut pos: u64 = 0;
    let mut nested: Option<String> = None;
    let mut super_member: Option<(String, u64, u64)> = None; // name, offset, size

    while pos + 512 <= total {
        f.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 512];
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        if hdr.iter().all(|&b| b == 0) {
            break; // end of archive
        }
        let name = tar_name(&hdr);
        let size = tar_octal(&hdr[124..136]);
        let data_off = pos + 512;
        if is_super_member(&name) {
            super_member = Some((name.clone(), data_off, size));
            break; // super wins outright
        } else if nested.is_none() && is_nested_archive(&name) {
            nested = Some(name.clone());
            let out = workdir.join(sanitize(&name));
            extract_range(&mut f, data_off, size, &out)?;
            // Recurse immediately; if it yields a super we're done.
            if let Ok(raw) = to_raw_super(&out, workdir, log) {
                return Ok(raw);
            }
        }
        // advance: header + data padded to 512
        pos = data_off + ((size + 511) / 512) * 512;
    }

    if let Some((name, off, size)) = super_member {
        let out = workdir.join(sanitize(&name));
        extract_range(&mut f, off, size, &out)?;
        log.info(&format!("extracted {name} from tar"));
        return to_raw_super(&out, workdir, log);
    }
    bail!("no super.img(.lz4) or nested AP archive found in {}", input.display());
}

fn extract_from_zip(input: &Path, workdir: &Path, log: &Logger) -> Result<PathBuf> {
    let file = std::fs::File::open(input)?;
    let mut zip = zip::ZipArchive::new(file).context("open zip")?;
    // Prefer a direct super member; else a nested AP archive.
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    names.sort();
    let pick = names
        .iter()
        .find(|n| is_super_member(n))
        .or_else(|| names.iter().find(|n| is_nested_archive(n)))
        .cloned()
        .with_context(|| format!("no super or AP archive in {}", input.display()))?;
    log.info(&format!("zip member: {pick}"));
    let out = workdir.join(sanitize(&pick));
    {
        let mut entry = zip.by_name(&pick)?;
        let mut w = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut w)?;
    }
    to_raw_super(&out, workdir, log)
}

fn extract_range(f: &mut std::fs::File, off: u64, size: u64, out: &Path) -> Result<()> {
    f.seek(SeekFrom::Start(off))?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut remaining = size;
    let mut buf = vec![0u8; 1 << 20];
    use std::io::Write;
    while remaining > 0 {
        let n = (remaining.min(buf.len() as u64)) as usize;
        f.read_exact(&mut buf[..n])?;
        w.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    w.flush()?;
    Ok(())
}

fn tar_name(hdr: &[u8]) -> String {
    let name = cstr(&hdr[0..100]);
    let prefix = cstr(&hdr[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}
fn tar_octal(b: &[u8]) -> u64 {
    let s: String = b
        .iter()
        .take_while(|&&c| c != 0 && c != b' ')
        .map(|&c| c as char)
        .collect();
    u64::from_str_radix(s.trim(), 8).unwrap_or(0)
}
fn sanitize(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_string()
}
