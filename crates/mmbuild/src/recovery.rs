// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! `mmbuild recovery` — build a custom recovery image.
//!
//! We do NOT ship a modified stock recovery. Instead we take the stock
//! recovery.img purely for its kernel + dtb (and boot header geometry), and
//! swap in a brand-new minimal initramfs whose `/init` is our own static
//! recovery binary (built by the `recovery` sub-project). The result boots the
//! mmgui recovery menu instead of Android recovery.
//!
//! Outputs (under build/):
//!   * recovery.img            — the repacked image (no AVB footer)
//!   * vbmeta_disabled.img     — stock vbmeta with verification flags set, so an
//!                               unlocked bootloader accepts the unsigned image
//!   * <device>_Recovery.tar   — Odin-flashable (recovery.img.lz4 + vbmeta.img.lz4)
//!
//! Verification can only be disabled, never re-signed — we do not have Samsung's
//! AVB key. Flashing therefore requires an unlocked bootloader (OEM unlock), and
//! the printed steps say so.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use mmcore::log::Logger;
use superimg::bootimg::BootImage;
use superimg::cpio::{self, Entry};
use superimg::{lz4frame, tarball};

use crate::config::{BuildConfig, Settings};
use crate::subproject;

#[allow(dead_code)] // vbmeta/tar are part of the result API; the CLI reads .img
pub struct RecoveryArtifacts {
    pub img: PathBuf,
    pub vbmeta: Option<PathBuf>,
    pub tar: Option<PathBuf>,
}

/// Directories the recovery initramfs must provide as mountpoints. recovery_main
/// cold-boots /dev and mounts proc/sys/tmpfs itself, but the mountpoints have to
/// exist in the ramdisk root first.
const RAMDISK_DIRS: &[&str] = &[
    "dev", "proc", "sys", "tmp", "mnt", "cache", "data", "system", "sdcard",
];

pub fn build(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    device: &str,
    log: &Logger,
) -> Result<RecoveryArtifacts> {
    let work = resolve(root, &settings.paths.work).join("recovery");
    std::fs::create_dir_all(&work)?;
    let build_dir = root.join("build");
    std::fs::create_dir_all(&build_dir)?;

    // 1. Build our recovery init (the `recovery` sub-project is `manual`, so
    //    name it explicitly). Produces build/recovery_init.
    log.task_start("mmbuild:buildRecoveryInit");
    subproject::build_all(root, settings, log, Some("recovery"), true)
        .context("build recovery sub-project")?;
    let init_bin = build_dir.join("recovery_init");
    if !init_bin.exists() {
        bail!("recovery sub-project did not produce {}", init_bin.display());
    }
    log.task_done("mmbuild:buildRecoveryInit", mmcore::log::Outcome::Executed);

    // 2. Obtain the stock recovery.img (for kernel + dtb + geometry).
    log.task_start("mmbuild:extractStockRecovery");
    let ap = resolve(root, &cfg.firmware.local_ap);
    if !ap.exists() {
        bail!(
            "firmware AP not found at {} (set [firmware].local_ap)",
            ap.display()
        );
    }
    let stock = work.join("recovery_stock.img");
    extract_image(&ap, "recovery", &stock, log)
        .context("extract stock recovery.img from AP")?;
    let stock_bytes = std::fs::read(&stock)?;
    let mut img = BootImage::parse(&stock_bytes).context("parse stock recovery.img")?;
    log.info(&format!(
        "stock recovery: header v{}, page {}, kernel {}, ramdisk {}, dtb {}, cmdline {:?}",
        img.header_version,
        img.page_size,
        mmcore::human_bytes(img.kernel.len() as u64),
        mmcore::human_bytes(img.ramdisk.len() as u64),
        mmcore::human_bytes(img.dtb.len() as u64),
        img.cmdline,
    ));
    log.task_done("mmbuild:extractStockRecovery", mmcore::log::Outcome::Executed);

    // 3. Build our minimal initramfs and gzip it.
    log.task_start("mmbuild:buildRamdisk");
    let init_data = std::fs::read(&init_bin)?;
    let ramdisk = build_ramdisk(&init_data);
    let gz = gzip(&ramdisk)?;
    log.info(&format!(
        "recovery ramdisk: {} file(s), {} → {} gz",
        RAMDISK_DIRS.len() + 1,
        mmcore::human_bytes(ramdisk.len() as u64),
        mmcore::human_bytes(gz.len() as u64),
    ));
    log.task_done("mmbuild:buildRamdisk", mmcore::log::Outcome::Executed);

    // 4. Swap the ramdisk and rebuild the boot image.
    log.task_start("mmbuild:repackRecovery");
    img.ramdisk = gz;
    let out_bytes = img.build();
    let out_img = build_dir.join("recovery.img");
    std::fs::write(&out_img, &out_bytes)?;
    log.info(&format!(
        "built {} ({})",
        out_img.display(),
        mmcore::human_bytes(out_bytes.len() as u64)
    ));
    log.task_done("mmbuild:repackRecovery", mmcore::log::Outcome::Executed);

    // 5. Patch vbmeta to disable verification (best-effort; AP may not ship it).
    log.task_start("mmbuild:patchVbmeta");
    let vbmeta_out = match extract_image(&ap, "vbmeta", &work.join("vbmeta_stock.img"), log) {
        Ok(()) => {
            let stock_vb = std::fs::read(work.join("vbmeta_stock.img"))?;
            let patched = patch_vbmeta(&stock_vb)?;
            let p = build_dir.join("vbmeta_disabled.img");
            std::fs::write(&p, &patched)?;
            log.info(&format!(
                "patched vbmeta → {} (verification flags disabled)",
                p.display()
            ));
            Some(p)
        }
        Err(e) => {
            log.warn(&format!(
                "vbmeta not patched: {e:#}. You may need `fastboot --disable-verification` instead."
            ));
            None
        }
    };
    log.task_done("mmbuild:patchVbmeta", mmcore::log::Outcome::Executed);

    // 6. Odin-flashable tar (lz4 members, matching Samsung's AP layout).
    log.task_start("mmbuild:packRecoveryTar");
    let tar = pack_odin_tar(&build_dir, &out_img, vbmeta_out.as_deref(), device, log)?;
    log.task_done("mmbuild:packRecoveryTar", mmcore::log::Outcome::Executed);

    print_flash_steps(log, &out_img, vbmeta_out.as_deref(), &tar);

    Ok(RecoveryArtifacts {
        img: out_img,
        vbmeta: vbmeta_out,
        tar: Some(tar),
    })
}

/// Build the initramfs cpio: the standard mountpoint dirs + our /init (0755).
fn build_ramdisk(init: &[u8]) -> Vec<u8> {
    let mut entries = Vec::new();
    for d in RAMDISK_DIRS {
        entries.push(Entry::dir(d, 0o755));
    }
    entries.push(Entry::file("init", 0o755, init.to_vec()));
    cpio::write(&entries)
}

fn gzip(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(6));
    enc.write_all(data).context("gzip write")?;
    enc.finish().context("gzip finish")
}

/// Set the AVB "verification disabled" flags on a top-level vbmeta image.
/// The vbmeta header is `AVB0` at offset 0; the `flags` field is a big-endian
/// u32 at offset 120 in AvbVBMetaImageHeader. Magisk/avbtool set bit0 (HASHTREE
/// disabled) and bit1 (VERIFICATION disabled) → 0x3.
fn patch_vbmeta(stock: &[u8]) -> Result<Vec<u8>> {
    if stock.len() < 128 || &stock[0..4] != b"AVB0" {
        bail!("not a vbmeta image (missing AVB0 magic)");
    }
    let mut out = stock.to_vec();
    // AvbVBMetaImageHeader: magic[4], required_libavb_version_major(u32),
    // _minor(u32), authentication_data_block_size(u64),
    // auxiliary_data_block_size(u64), algorithm_type(u32), then hash/sig
    // offsets+sizes (4×u64), then public_key/metadata (4×u64), rollback_index
    // (u64), flags(u32) at offset 120, then rollback_index_location(u32)...
    // The flags field is at byte offset 120 (big-endian u32).
    let off = 120;
    let cur = u32::from_be_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]);
    let new = cur | 0x3;
    out[off..off + 4].copy_from_slice(&new.to_be_bytes());
    Ok(out)
}

/// Pack recovery.img (+ vbmeta) into an Odin AP-style tar with lz4 members.
fn pack_odin_tar(
    build_dir: &Path,
    img: &Path,
    vbmeta: Option<&Path>,
    device: &str,
    log: &Logger,
) -> Result<PathBuf> {
    let rec_lz4 = build_dir.join("recovery.img.lz4");
    lz4frame::compress_file(img, &rec_lz4, None)?;
    let vb_lz4 = match vbmeta {
        Some(v) => {
            let p = build_dir.join("vbmeta.img.lz4");
            lz4frame::compress_file(v, &p, None)?;
            Some(p)
        }
        None => None,
    };

    let tar_path = build_dir.join(format!("{device}_Recovery.tar"));
    let f = std::fs::File::create(&tar_path)?;
    let mut tb = tarball::TarBuilder::new(std::io::BufWriter::new(f));
    tb.append_path("recovery.img.lz4", &rec_lz4)?;
    if let Some(v) = &vb_lz4 {
        tb.append_path("vbmeta.img.lz4", v)?;
    }
    tb.finish()?;
    log.info(&format!(
        "packed {} ({})",
        tar_path.display(),
        mmcore::human_bytes(std::fs::metadata(&tar_path)?.len())
    ));
    Ok(tar_path)
}

fn print_flash_steps(log: &Logger, img: &Path, vbmeta: Option<&Path>, tar: &Path) {
    log.lifecycle("recovery build complete — flashing (unlocked bootloader required):");
    log.lifecycle(&format!("  Odin:   AP slot → {}", tar.display()));
    log.lifecycle(&format!(
        "  heimdall: heimdall flash --RECOVERY {}",
        img.display()
    ));
    log.lifecycle(&format!(
        "  fastboot: fastboot flash recovery {}",
        img.display()
    ));
    match vbmeta {
        Some(v) => log.lifecycle(&format!(
            "  then disable verification: fastboot flash vbmeta {} (or flash via the AP tar)",
            v.display()
        )),
        None => log.lifecycle(
            "  then: fastboot --disable-verity --disable-verification flash vbmeta <stock vbmeta>",
        ),
    }
    log.lifecycle(
        "  NOTE: unsigned — the bootloader must be OEM-unlocked. We cannot sign with Samsung's AVB key.",
    );
}

// ----------------------- AP tar member extraction -----------------------

/// Extract `<name>.img` (or `<name>.img.lz4`) from a Samsung AP `.tar(.md5)` and
/// write the decompressed raw image to `out`. Shared with the main build pipeline
/// so `mmbuild build --target boot` can pull the stock boot.img from the same AP.
pub(crate) fn extract_image(ap: &Path, name: &str, out: &Path, log: &Logger) -> Result<()> {
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
            break; // reached the appended md5 / end of tar stream
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
        pos = data_off + ((size + 511) / 512) * 512;
    }
    bail!("{want_raw}(.lz4) not found in {}", ap.display())
}

fn extract_range(f: &mut std::fs::File, off: u64, size: u64, out: &Path) -> Result<()> {
    use std::io::Write;
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

fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}
