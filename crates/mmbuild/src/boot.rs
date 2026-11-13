// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! `mmbuild boot`: package boot.img + Odin tar.
//!
//! Default for the flashable System_OS tar is **verbatim stock boot.img**
//! (Micro BIOS runs soft inside `/system/bin/init`). Custom ramdisk bios
//! inject is kept in [`build_bios`] for bring-up work; on SM-A125F the
//! Magisk-style handoff does not complete.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;
use superimg::bootimg::BootImage;

use crate::android_img;
use crate::config::{BuildConfig, Settings};
use crate::subproject;

#[allow(dead_code)]
pub struct BootArtifacts {
    pub img: PathBuf,
    pub vbmetas: Vec<(String, PathBuf)>,
    pub tar: Option<PathBuf>,
}

/// Flashable default: stock boot bytes untouched + patched vbmeta.
pub fn build(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    device: &str,
    log: &Logger,
) -> Result<BootArtifacts> {
    build_stock(root, cfg, settings, device, log)
}

/// Verbatim stock `boot.img` from the AP (no ramdisk rewrite).
pub fn build_stock(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    device: &str,
    log: &Logger,
) -> Result<BootArtifacts> {
    let work = android_img::resolve(root, &settings.paths.work).join("boot");
    std::fs::create_dir_all(&work)?;
    let build_dir = root.join("build");
    std::fs::create_dir_all(&build_dir)?;

    log.task_start("mmbuild:extractStockBoot");
    let cache = android_img::resolve(root, &settings.paths.cache);
    let ap = android_img::resolve_ap(root, &cfg.firmware.local_ap, &cache, &cfg.device.model)
        .context("locate stock AP for boot")?;
    log.detail(&format!("stock AP: {}", ap.display()));
    let stock = work.join("boot_stock.img");
    android_img::extract_image(&ap, "boot", &stock, log)
        .context("extract stock boot.img from AP")?;
    let stock_bytes = std::fs::read(&stock)?;
    log.info(&format!(
        "stock boot verbatim {} (no bios inject — soft Micro BIOS in /system/bin/init)",
        mmcore::human_bytes(stock_bytes.len() as u64)
    ));
    log.task_done("mmbuild:extractStockBoot", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:repackBoot");
    let out_img = build_dir.join("boot.img");
    std::fs::write(&out_img, &stock_bytes)?;
    log.task_done("mmbuild:repackBoot", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:patchVbmeta");
    let vbmetas = match android_img::patch_vbmetas_from_ap(&ap, &work, &build_dir, log) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => {
            log.warn("no vbmeta images found in AP");
            Vec::new()
        }
        Err(e) => {
            log.warn(&format!("vbmeta not patched: {e:#}"));
            Vec::new()
        }
    };
    log.task_done("mmbuild:patchVbmeta", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:packBootTar");
    let tar =
        android_img::pack_odin_tar(&build_dir, &out_img, "boot", &vbmetas, device, "Boot", log)?;
    log.task_done("mmbuild:packBootTar", mmcore::log::Outcome::Executed);

    log.lifecycle("boot build complete:");
    log.lifecycle("  STOCK boot.img (byte-identical to AP) + patched vbmeta.");
    log.lifecycle("  Micro BIOS runs soft inside /system/bin/init (no ramdisk gate).");
    log.lifecycle(&format!("  Odin:   AP slot → {}", tar.display()));

    Ok(BootArtifacts {
        img: out_img,
        vbmetas,
        tar: Some(tar),
    })
}

/// Magisk-style bios inject. Not wired to the CLI; handoff fails on A125F.
#[allow(dead_code)]
pub fn build_bios(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    device: &str,
    log: &Logger,
) -> Result<BootArtifacts> {
    let work = android_img::resolve(root, &settings.paths.work).join("boot");
    std::fs::create_dir_all(&work)?;
    let build_dir = root.join("build");
    std::fs::create_dir_all(&build_dir)?;

    log.task_start("mmbuild:buildBios");
    subproject::build_all(root, settings, log, Some("bios"), true, &cfg.device.model)
        .context("build bios sub-project")?;
    let init_bin = build_dir.join("bios");
    if !init_bin.exists() {
        bail!("bios sub-project did not produce {}", init_bin.display());
    }
    log.task_done("mmbuild:buildBios", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:extractStockBoot");
    let cache = android_img::resolve(root, &settings.paths.cache);
    let ap = android_img::resolve_ap(root, &cfg.firmware.local_ap, &cache, &cfg.device.model)
        .context("locate stock AP for boot")?;
    let stock = work.join("boot_stock.img");
    android_img::extract_image(&ap, "boot", &stock, log)
        .context("extract stock boot.img from AP")?;
    let stock_bytes = std::fs::read(&stock)?;
    let mut img = BootImage::parse(&stock_bytes).context("parse stock boot.img")?;
    log.task_done("mmbuild:extractStockBoot", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:buildBootRamdisk");
    let init_data = std::fs::read(&init_bin)?;
    let (ramdisk, nfiles) = android_img::inject_init_keep_stock(&img.ramdisk, &init_data)
        .context("inject bios into stock ramdisk")?;
    let gz = android_img::gzip_ramdisk(&ramdisk)?;
    log.info(&format!(
        "EXPERIMENTAL bios inject: {} entries, {} gz",
        nfiles,
        mmcore::human_bytes(gz.len() as u64)
    ));
    log.task_done("mmbuild:buildBootRamdisk", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:repackBoot");
    img.ramdisk = gz;
    let out_bytes = img.build_padded(Some(stock_bytes.len()));
    let out_img = build_dir.join("boot_bios.img");
    std::fs::write(&out_img, &out_bytes)?;
    // Also write boot.img so pack_odin_tar naming stays consistent when called alone.
    std::fs::write(build_dir.join("boot.img"), &out_bytes)?;
    log.task_done("mmbuild:repackBoot", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:patchVbmeta");
    let vbmetas = match android_img::patch_vbmetas_from_ap(&ap, &work, &build_dir, log) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => Vec::new(),
        Err(e) => {
            log.warn(&format!("vbmeta not patched: {e:#}"));
            Vec::new()
        }
    };
    log.task_done("mmbuild:patchVbmeta", mmcore::log::Outcome::Executed);

    log.task_start("mmbuild:packBootTar");
    let tar = android_img::pack_odin_tar(
        &build_dir, &out_img, "boot", &vbmetas, device, "BootBios", log,
    )?;
    log.task_done("mmbuild:packBootTar", mmcore::log::Outcome::Executed);

    log.warn("bios-injected boot is experimental — prefer stock boot + soft Micro BIOS");
    Ok(BootArtifacts {
        img: out_img,
        vbmetas,
        tar: Some(tar),
    })
}
