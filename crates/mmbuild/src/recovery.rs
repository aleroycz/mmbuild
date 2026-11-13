// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! `mmbuild recovery`: repack recovery.img with recovery_init ramdisk + vbmeta + Odin tar.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;
use superimg::bootimg::BootImage;

use crate::android_img;
use crate::config::{BuildConfig, Settings};
use crate::subproject;

#[allow(dead_code)] // vbmeta/tar are part of the result API; the CLI reads .img
pub struct RecoveryArtifacts {
    pub img: PathBuf,
    /// Patched vbmeta images as `(tar member stem like "vbmeta.img", path)`.
    pub vbmetas: Vec<(String, PathBuf)>,
    pub tar: Option<PathBuf>,
}

pub fn build(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    device: &str,
    log: &Logger,
) -> Result<RecoveryArtifacts> {
    let work = android_img::resolve(root, &settings.paths.work).join("recovery");
    std::fs::create_dir_all(&work)?;
    let build_dir = root.join("build");
    std::fs::create_dir_all(&build_dir)?;

    // 1. Build our recovery init (the `recovery` sub-project is `manual`, so name it explicitly).
    // Produces build/recovery_init.
    log.task_start("mmbuild:buildRecoveryInit");
    subproject::build_all(
        root,
        settings,
        log,
        Some("recovery"),
        true,
        &cfg.device.model,
    )
    .context("build recovery sub-project")?;
    let init_bin = build_dir.join("recovery_init");
    if !init_bin.exists() {
        bail!(
            "recovery sub-project did not produce {}",
            init_bin.display()
        );
    }
    log.task_done("mmbuild:buildRecoveryInit", mmcore::log::Outcome::Executed);

    // 2. Obtain the stock recovery.img (for kernel + dtb + geometry).
    log.task_start("mmbuild:extractStockRecovery");
    let cache = android_img::resolve(root, &settings.paths.cache);
    let ap = android_img::resolve_ap(root, &cfg.firmware.local_ap, &cache, &cfg.device.model)
        .context("locate stock AP for recovery")?;
    log.detail(&format!("stock AP: {}", ap.display()));
    let stock = work.join("recovery_stock.img");
    android_img::extract_image(&ap, "recovery", &stock, log)
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
    log.task_done(
        "mmbuild:extractStockRecovery",
        mmcore::log::Outcome::Executed,
    );

    // 3. Inject recovery_init into the stock recovery ramdisk (Magisk-style).
    // Stock /init is a symlink -> /system/bin/init; the ramdisk carries the
    // full recovery userspace. Do NOT replace it with a tiny empty cpio.
    log.task_start("mmbuild:buildRamdisk");
    let init_data = std::fs::read(&init_bin)?;
    let (ramdisk, nfiles) = android_img::inject_init_keep_stock(&img.ramdisk, &init_data)
        .context("inject recovery_init into stock recovery ramdisk")?;
    let gz = android_img::gzip_ramdisk(&ramdisk)?;
    log.info(&format!(
        "recovery ramdisk: {} entries, {} → {} gz (stock kept; /.backup/init = old /init)",
        nfiles,
        mmcore::human_bytes(ramdisk.len() as u64),
        mmcore::human_bytes(gz.len() as u64),
    ));
    log.task_done("mmbuild:buildRamdisk", mmcore::log::Outcome::Executed);

    // 4. Swap the ramdisk and rebuild. `build_padded` recomputes
    // recovery_dtbo_offset (stock offset is wrong after a smaller ramdisk),
    // re-appends MagiskBoot's SEANDROIDENFORCE + SignerVer02 tail, and pads
    // to the stock 32 MiB partition size. AVB footer is dropped; flash the
    // patched vbmeta alongside.
    log.task_start("mmbuild:repackRecovery");
    img.ramdisk = gz;
    if img.samsung_tail.is_empty() {
        log.warn("stock recovery missing SEANDROIDENFORCE tail");
    } else {
        log.info(&format!(
            "preserving MagiskBoot Samsung tail ({} bytes)",
            img.samsung_tail.len()
        ));
    }
    let out_bytes = img.build_padded(Some(stock_bytes.len()));
    let out_img = build_dir.join("recovery.img");
    std::fs::write(&out_img, &out_bytes)?;
    log.info(&format!(
        "built {} (padded to stock {})",
        out_img.display(),
        mmcore::human_bytes(out_bytes.len() as u64)
    ));
    log.task_done("mmbuild:repackRecovery", mmcore::log::Outcome::Executed);

    // 5. Patch vbmeta + vbmeta_samsung (Magisk-compatible flags=0x3).
    log.task_start("mmbuild:patchVbmeta");
    let vbmetas = match android_img::patch_vbmetas_from_ap(&ap, &work, &build_dir, log) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => {
            log.warn("no vbmeta images found in AP");
            Vec::new()
        }
        Err(e) => {
            log.warn(&format!(
                "vbmeta not patched: {e:#}. You may need `fastboot --disable-verification` instead."
            ));
            Vec::new()
        }
    };
    log.task_done("mmbuild:patchVbmeta", mmcore::log::Outcome::Executed);

    // 6. Odin-flashable tar (lz4 members, matching Samsung's AP layout).
    log.task_start("mmbuild:packRecoveryTar");
    let tar = android_img::pack_odin_tar(
        &build_dir, &out_img, "recovery", &vbmetas, device, "Recovery", log,
    )?;
    log.task_done("mmbuild:packRecoveryTar", mmcore::log::Outcome::Executed);

    print_flash_steps(log, &out_img, &vbmetas, &tar);

    Ok(RecoveryArtifacts {
        img: out_img,
        vbmetas,
        tar: Some(tar),
    })
}

fn print_flash_steps(log: &Logger, img: &Path, vbmetas: &[(String, PathBuf)], tar: &Path) {
    log.lifecycle("recovery build complete:");
    log.lifecycle("  Magisk-style inject; SEANDROIDENFORCE + SignerVer02 preserved, AVBf dropped.");
    log.lifecycle(&format!("  Odin:   AP slot → {}", tar.display()));
    log.lifecycle(&format!(
        "  heimdall: heimdall flash --RECOVERY {}",
        img.display()
    ));
    log.lifecycle(&format!(
        "  fastboot: fastboot flash recovery {}",
        img.display()
    ));
    if vbmetas.is_empty() {
        log.lifecycle(
            "  then: fastboot --disable-verity --disable-verification flash vbmeta <stock vbmeta>",
        );
    } else {
        for (stem, p) in vbmetas {
            let part = stem.trim_end_matches(".img");
            log.lifecycle(&format!(
                "  then: fastboot flash {part} {} (or flash via the AP tar)",
                p.display()
            ));
        }
    }
}
