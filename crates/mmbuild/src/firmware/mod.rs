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

//! Firmware acquisition: obtain a raw `super` image either by downloading from
//! Samsung's FUS server (samloader-style) or from a local file, per `build.toml`.

pub mod crypt;
pub mod fus;
pub mod unpack;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;

use crate::config::{BuildConfig, FirmwareSource, Settings};

/// Resolve `build.toml`'s firmware settings into a raw super image on disk.
pub fn acquire_raw_super(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    log: &Logger,
) -> Result<PathBuf> {
    let work = resolve(root, &settings.paths.work);
    std::fs::create_dir_all(&work).ok();
    match cfg.firmware.source {
        FirmwareSource::Local => from_local(root, cfg, &work, log),
        FirmwareSource::Download => from_fus(root, cfg, settings, &work, log),
        FirmwareSource::DownloadThenLocal => {
            match from_fus(root, cfg, settings, &work, log) {
                Ok(p) => Ok(p),
                Err(e) => {
                    log.warn(&format!("FUS download failed ({e}); falling back to local file"));
                    from_local(root, cfg, &work, log)
                }
            }
        }
    }
}

fn from_local(root: &Path, cfg: &BuildConfig, work: &Path, log: &Logger) -> Result<PathBuf> {
    if cfg.firmware.local_ap.is_empty() {
        bail!("firmware source needs a local file but [firmware].local_ap is empty");
    }
    let p = resolve(root, &cfg.firmware.local_ap);
    if !p.exists() {
        bail!("local firmware not found: {}", p.display());
    }
    log.info(&format!("using local firmware: {}", p.display()));
    unpack::to_raw_super(&p, work, log)
}

fn from_fus(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    work: &Path,
    log: &Logger,
) -> Result<PathBuf> {
    if cfg.device.model.is_empty() || cfg.device.region.is_empty() {
        bail!("FUS download needs [device].model and [device].region in build.toml");
    }
    let cache = resolve(root, &settings.paths.cache);
    std::fs::create_dir_all(&cache).ok();

    log.detail("FUS: handshake");
    let mut client = fus::FusClient::new().context("FUS nonce handshake")?;
    let version = client.latest_version(&cfg.device.model, &cfg.device.region)?;
    log.info(&format!(
        "latest firmware for {}/{}: {version}",
        cfg.device.model, cfg.device.region
    ));
    let info = client.binary_inform(&version, &cfg.device.model, &cfg.device.region)?;
    log.info(&format!(
        "download: {} ({})",
        info.filename,
        mmcore::human_bytes(info.size)
    ));
    client.binary_init(&info.filename)?;

    let enc_path = cache.join(&info.filename);
    let remote = format!("{}{}", info.model_path, info.filename);
    {
        let mut cb = |c: u64, t: u64| log.op_progress(c, t);
        client.download(&remote, &enc_path, info.size, Some(&mut cb))?;
    }

    // Decrypt .enc4 / .enc2.
    let key = if info.is_enc2() {
        crypt::v2_key(&cfg.device.region, &cfg.device.model, &version)
    } else {
        crypt::v4_key(&info.fw_version, &info.logic_value)?
    };
    let dec_path = cache.join(strip_enc(&info.filename));
    log.detail("decrypting firmware");
    {
        let mut cb = |c: u64, t: u64| log.op_progress(c, t);
        crypt::decrypt_file(&enc_path, &dec_path, &key, Some(&mut cb))?;
    }
    if !cfg.firmware.keep_download {
        std::fs::remove_file(&enc_path).ok();
    }
    unpack::to_raw_super(&dec_path, work, log)
}

fn strip_enc(name: &str) -> String {
    name.trim_end_matches(".enc4")
        .trim_end_matches(".enc2")
        .to_string()
}

fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}
