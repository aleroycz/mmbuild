// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! Shared boot/recovery image helpers: ramdisk cpio, vbmeta, AP tar extract/pack.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use mmcore::log::Logger;
use superimg::cpio::{self, Entry};
use superimg::{lz4frame, tarball};

/// Mountpoint directories for a from-scratch initramfs (tests / fallback only).
#[allow(dead_code)]
pub const RAMDISK_DIRS: &[&str] = &[
    "dev", "proc", "sys", "tmp", "mnt", "cache", "data", "system", "sdcard",
];

/// Build a minimal newc initramfs: mountpoint dirs + `/init` (0755).
/// Boot/recovery packaging uses [`inject_init_keep_stock`] instead.
#[allow(dead_code)]
pub fn build_minimal_ramdisk(init: &[u8]) -> Vec<u8> {
    let mut entries = Vec::new();
    for d in RAMDISK_DIRS {
        entries.push(Entry::dir(d, 0o755));
    }
    entries.push(Entry::file("init", 0o755, init.to_vec()));
    cpio::write(&entries)
}

/// Patch stock boot/recovery ramdisk Magisk-style:
/// stock `/init` -> `/.backup/init`, `our_init` becomes `/init`.
///
/// **Boot:** stock `/init` is a real first-stage ELF. Bios restores the backup
/// then `execve("/init")` - required for 2SI / logical `super` mounts.
///
/// **Recovery (SM-A125F):** stock `/init` is a symlink to `/system/bin/init`
/// inside a full recovery ramdisk (linker, libs, fstab, sepolicy). Replacing
/// that entire ramdisk with a tiny cpio breaks bring-up; keep stock contents
/// and only swap `/init` for our static `recovery_init`.
pub fn inject_init_keep_stock(stock_ramdisk: &[u8], our_init: &[u8]) -> Result<(Vec<u8>, usize)> {
    let raw = decompress_ramdisk(stock_ramdisk)?;
    let mut entries = cpio::read(&raw).context("parse stock ramdisk cpio")?;

    // Drop leftovers from older packagers / prior runs (not stock /init yet).
    entries.retain(|e| {
        let n = e.name.trim_start_matches("./");
        n != "init.android" && n != ".backup/init" && n != ".backup"
    });

    let init_idx = entries
        .iter()
        .position(|e| e.name.trim_start_matches("./") == "init")
        .context("stock ramdisk has no /init")?;

    // Magisk `cpio mv init .backup/init`: rename in place (keep inode/order),
    // then `cpio add` our binary as the new /init at the end.
    entries.insert(init_idx, Entry::dir(".backup", 0o755));
    let backup_idx = init_idx + 1;
    entries[backup_idx].name = ".backup/init".into();
    // 0750 matches stock boot init / recovery symlink mode bits.
    entries.push(Entry::file("init", 0o750, our_init.to_vec()));

    Ok((cpio::write(&entries), entries.len()))
}

/// Decompress a boot/recovery ramdisk blob (gzip, lz4 frame, or raw cpio).
pub fn decompress_ramdisk(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
        let mut dec = GzDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)
            .context("gzip decompress ramdisk")?;
        return Ok(out);
    }
    if lz4frame::is_lz4_bytes(data) {
        return lz4frame::decompress_bytes(data).context("lz4 decompress ramdisk");
    }
    if data.len() >= 6 && &data[0..6] == b"070701" {
        return Ok(data.to_vec());
    }
    bail!("unknown ramdisk compression (not gzip/lz4/cpio)")
}

/// Gzip a ramdisk (level 6) - what Samsung recovery/boot images expect.
pub fn gzip_ramdisk(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(6));
    enc.write_all(data).context("gzip write")?;
    enc.finish().context("gzip finish")
}

/// AVB flags Magisk / `fastboot --disable-verity --disable-verification` set:
/// bit0 = HASHTREE_DISABLED, bit1 = VERIFICATION_DISABLED.
pub const VBMETA_FLAGS_DISABLED: u32 = 0x3;

/// Stems to pull from the stock AP (SM-A125F ships both).
pub const VBMETA_STEMS: &[&str] = &["vbmeta", "vbmeta_samsung"];

/// Set AVB disable-verity + disable-verification flags (Magisk-compatible).
///
/// Only the big-endian `flags` field at offset 120 is changed. Unlocked
/// Samsung bootloaders treat the resulting header hash mismatch as allowed
/// and then honour `VERIFICATION_DISABLED` for boot/recovery without footers.
pub fn patch_vbmeta(stock: &[u8]) -> Result<Vec<u8>> {
    if stock.len() < 256 || &stock[0..4] != b"AVB0" {
        bail!("not a vbmeta image (missing AVB0 magic)");
    }
    let mut out = stock.to_vec();
    let off = 120;
    // Magisk writes absolute 3; OR keeps any future vendor bits if present.
    let cur = u32::from_be_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]);
    let new = cur | VBMETA_FLAGS_DISABLED;
    out[off..off + 4].copy_from_slice(&new.to_be_bytes());
    Ok(out)
}

/// Extract every known vbmeta from `ap`, patch flags, write under `build_dir`.
///
/// Returns `(tar_member_name, image_path)` pairs, e.g.
/// `("vbmeta.img", build/vbmeta_disabled.img)`.
pub fn patch_vbmetas_from_ap(
    ap: &Path,
    work: &Path,
    build_dir: &Path,
    log: &Logger,
) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for stem in VBMETA_STEMS {
        let stock_path = work.join(format!("{stem}_stock.img"));
        match extract_image(ap, stem, &stock_path, log) {
            Ok(()) => {
                let stock = std::fs::read(&stock_path)
                    .with_context(|| format!("read {}", stock_path.display()))?;
                let patched = patch_vbmeta(&stock).with_context(|| format!("patch {stem}"))?;
                let flags =
                    u32::from_be_bytes([patched[120], patched[121], patched[122], patched[123]]);
                let name = if *stem == "vbmeta" {
                    "vbmeta_disabled.img".to_string()
                } else {
                    format!("{stem}_disabled.img")
                };
                let p = build_dir.join(&name);
                std::fs::write(&p, &patched)?;
                log.info(&format!(
                    "patched {stem} → {} (flags=0x{flags:x}, {})",
                    p.display(),
                    mmcore::human_bytes(patched.len() as u64)
                ));
                out.push((format!("{stem}.img"), p));
            }
            Err(e) => {
                if *stem == "vbmeta" {
                    return Err(e).context("extract stock vbmeta");
                }
                log.detail(&format!("skip {stem}: {e:#}"));
            }
        }
    }
    Ok(out)
}

/// Pack `{name}.img.lz4` (+ optional patched vbmeta members) into an Odin AP tar.
pub fn pack_odin_tar(
    build_dir: &Path,
    img: &Path,
    member_stem: &str,
    vbmetas: &[(String, PathBuf)],
    device: &str,
    tar_suffix: &str,
    log: &Logger,
) -> Result<PathBuf> {
    let img_lz4 = build_dir.join(format!("{member_stem}.img.lz4"));
    lz4frame::compress_file(img, &img_lz4, None)?;

    let tar_path = build_dir.join(format!("{device}_{tar_suffix}.tar"));
    let f = std::fs::File::create(&tar_path)?;
    let mut tb = tarball::TarBuilder::new(std::io::BufWriter::new(f));
    let member = format!("{member_stem}.img.lz4");
    tb.append_path(&member, &img_lz4)?;
    for (stem_img, path) in vbmetas {
        // stem_img is "vbmeta.img" / "vbmeta_samsung.img" -> member "*.img.lz4"
        let lz_name = format!("{stem_img}.lz4");
        let lz_path = build_dir.join(&lz_name);
        lz4frame::compress_file(path, &lz_path, None)?;
        tb.append_path(&lz_name, &lz_path)?;
    }
    tb.finish()?;
    log.info(&format!(
        "packed {} ({})",
        tar_path.display(),
        mmcore::human_bytes(std::fs::metadata(&tar_path)?.len())
    ));
    Ok(tar_path)
}

/// Extract `<name>.img` (or `<name>.img.lz4`) from a Samsung AP `.tar(.md5)`.
pub fn extract_image(ap: &Path, name: &str, out: &Path, log: &Logger) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let want_raw = format!("{name}.img");
    let want_lz4 = format!("{name}.img.lz4");

    let mut f = std::fs::File::open(ap).with_context(|| format!("open {}", ap.display()))?;
    let total = f.metadata()?.len();
    let mut pos = 0u64;
    let mut hdr = [0u8; 512];
    while pos + 512 <= total {
        f.seek(SeekFrom::Start(pos))?;
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        if hdr.iter().all(|&b| b == 0) {
            pos += 512;
            continue;
        }
        if &hdr[257..262] != b"ustar" {
            break;
        }
        let member = tar_name(&hdr);
        let size = tar_octal(&hdr[124..136]);
        let data_off = pos + 512;
        let base = member.rsplit('/').next().unwrap_or(&member);
        if base == want_lz4 {
            let tmp = out.with_extension("img.lz4");
            extract_range(&mut f, data_off, size, &tmp)?;
            log.detail(&format!("lz4 decompress {member}"));
            lz4frame::decompress_file(&tmp, out, None)?;
            std::fs::remove_file(&tmp).ok();
            return Ok(());
        } else if base == want_raw {
            extract_range(&mut f, data_off, size, out)?;
            return Ok(());
        }
        pos = data_off + size.div_ceil(512) * 512;
    }
    bail!("{want_raw}(.lz4) not found in {}", ap.display())
}

fn extract_range(f: &mut std::fs::File, off: u64, size: u64, out: &Path) -> Result<()> {
    f.seek(SeekFrom::Start(off))?;
    let mut remaining = size;
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut buf = vec![0u8; 1 << 20];
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
    String::from_utf8_lossy(&b[..end]).to_string()
}
fn tar_octal(b: &[u8]) -> u64 {
    let s: String = b
        .iter()
        .map(|&c| c as char)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    u64::from_str_radix(s.trim(), 8).unwrap_or(0)
}

pub fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}

/// Locate a stock AP tar: prefer `[firmware].local_ap`, else extract `AP_*.tar.md5`
/// from a decrypted FUS factory zip in the firmware cache.
pub fn resolve_ap(root: &Path, local_ap: &str, cache_dir: &Path, model: &str) -> Result<PathBuf> {
    if !local_ap.trim().is_empty() {
        let p = resolve(root, local_ap);
        if p.is_file() {
            /* Direct AP tar, or a factory zip that wraps one. */
            if is_ap_tar_name(&p) {
                return Ok(p);
            }
            if is_factory_zip_name(&p) {
                return extract_ap_from_factory_zip(
                    &p,
                    &resolve(root, &cache_dir.to_string_lossy()),
                );
            }
        }
    }
    let cache = resolve(root, &cache_dir.to_string_lossy());
    if cache.is_dir() {
        let model_u = model.to_uppercase();
        /* Prefer an already-extracted AP_*.tar.md5. */
        let mut ap_tars: Vec<PathBuf> = Vec::new();
        let mut factory_zips: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&cache) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let name = file_name_upper(&p);
                if !model_u.is_empty() && !name.contains(&model_u) {
                    continue;
                }
                if is_ap_tar_name(&p) {
                    ap_tars.push(p);
                } else if is_factory_zip_name(&p) {
                    factory_zips.push(p);
                }
            }
        }
        ap_tars.sort();
        if let Some(p) = ap_tars.into_iter().next_back() {
            return Ok(p);
        }
        factory_zips.sort();
        if let Some(z) = factory_zips.into_iter().next_back() {
            return extract_ap_from_factory_zip(&z, &cache);
        }
    }
    bail!(
        "firmware AP not found (set [firmware].local_ap, or run a FUS download so \
         a decrypted factory zip lands in {})",
        cache.display()
    )
}

fn file_name_upper(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_uppercase()
}

fn is_ap_tar_name(p: &Path) -> bool {
    let n = file_name_upper(p);
    n.starts_with("AP_") && (n.ends_with(".TAR") || n.ends_with(".TAR.MD5") || n.ends_with(".MD5"))
}

fn is_factory_zip_name(p: &Path) -> bool {
    let n = file_name_upper(p);
    n.ends_with(".ZIP") && !n.ends_with(".ENC4") && !n.starts_with("AP_")
}

fn extract_ap_from_factory_zip(zip_path: &Path, cache: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(cache).ok();
    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("open factory zip {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("read factory zip {}", zip_path.display()))?;
    let mut ap_name: Option<String> = None;
    for i in 0..archive.len() {
        let e = archive.by_index(i)?;
        let name = e.name().to_string();
        let base = name.rsplit('/').next().unwrap_or(&name).to_uppercase();
        if base.starts_with("AP_")
            && (base.ends_with(".TAR") || base.ends_with(".TAR.MD5") || base.ends_with(".MD5"))
        {
            ap_name = Some(name);
            break;
        }
    }
    let Some(member) = ap_name else {
        bail!("no AP_*.tar.md5 inside factory zip {}", zip_path.display());
    };
    let base = member.rsplit('/').next().unwrap_or(&member);
    let out = cache.join(base);
    if out.is_file() && out.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(out);
    }
    let mut e = archive
        .by_name(&member)
        .with_context(|| format!("zip member {member}"))?;
    let mut w = std::io::BufWriter::new(std::fs::File::create(&out)?);
    std::io::copy(&mut e, &mut w)?;
    w.flush()?;
    Ok(out)
}
