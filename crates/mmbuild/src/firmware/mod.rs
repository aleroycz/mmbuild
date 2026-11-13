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

//! Resolve build.toml firmware settings to a raw super on disk.

pub mod crypt;
pub mod fus;
pub mod pixel;
pub mod unpack;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mmcore::log::Logger;

use crate::config::{BuildConfig, FirmwareSource, Settings};
use crate::fingerprint;

/// Result of firmware acquisition. `cache_hit` means a pristine cached super was reused (unpack /
/// download skipped);
pub struct Acquired {
    pub raw_super: PathBuf,
    pub cache_hit: bool,
}

/// Resolve `build.toml`'s firmware settings into a raw super image on disk.
pub fn acquire_raw_super(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    log: &Logger,
) -> Result<Acquired> {
    let work = resolve(root, &settings.paths.work);
    let cache = resolve(root, &settings.paths.cache);
    std::fs::create_dir_all(&work).ok();
    std::fs::create_dir_all(&cache).ok();

    if matches!(cfg.firmware.source, FirmwareSource::Local) {
        return from_local(root, cfg, &work, &cache, log);
    }

    // A direct URL overrides brand resolution (Xiaomi and other brands).
    if !cfg.firmware.url.trim().is_empty() {
        return with_local_fallback(
            from_url(cfg, &work, &cache, log),
            root,
            cfg,
            &work,
            &cache,
            log,
        );
    }

    let brand = cfg.device.brand.trim().to_ascii_lowercase();
    let dl = match brand.as_str() {
        "google" | "pixel" => from_pixel(cfg, &work, &cache, log),
        "samsung" | "" => from_fus(root, cfg, &cache, &work, log),
        other => bail!(
            "brand \"{other}\" has no built-in firmware backend; set \
             [firmware].url to a direct firmware/ROM URL, or [firmware].source \
             = \"local\" with [firmware].local_ap"
        ),
    };
    with_local_fallback(dl, root, cfg, &work, &cache, log)
}

/// Under DownloadThenLocal, a failed download retries the local file.
fn with_local_fallback(
    dl: Result<Acquired>,
    root: &Path,
    cfg: &BuildConfig,
    work: &Path,
    cache: &Path,
    log: &Logger,
) -> Result<Acquired> {
    match dl {
        Ok(p) => Ok(p),
        Err(e) if matches!(cfg.firmware.source, FirmwareSource::DownloadThenLocal) => {
            log.warn(&format!(
                "download failed ({e}); falling back to local file"
            ));
            from_local(root, cfg, work, cache, log)
        }
        Err(e) => Err(e),
    }
}

/// Fetch [firmware].url and unpack to raw super.
fn from_url(cfg: &BuildConfig, work: &Path, cache: &Path, log: &Logger) -> Result<Acquired> {
    let url = cfg.firmware.url.trim();
    let dest = work.join(url_filename(url));
    if dest.exists() {
        if let Ok(fp) = fingerprint::local_firmware_digest(&dest) {
            if let Some(hit) = fingerprint::lookup_pristine(cache, &fp) {
                log.info(&format!(
                    "firmware cache hit (url artifact): {}",
                    hit.display()
                ));
                return Ok(Acquired {
                    raw_super: hit,
                    cache_hit: true,
                });
            }
        }
    }
    log.info(&format!("downloading firmware: {url}"));
    download_to(url, &dest, log)?;
    unpack_and_cache(&dest, work, cache, log)
}

/// Resolve/download latest Pixel factory -> super.
fn from_pixel(cfg: &BuildConfig, work: &Path, cache: &Path, log: &Logger) -> Result<Acquired> {
    let codename = cfg.device.model.trim();
    if codename.is_empty() {
        bail!("Pixel download needs [device].model set to the codename (e.g. \"oriole\")");
    }
    log.detail("resolving latest Pixel factory image");
    let url = pixel::resolve_factory_url(codename)?;
    log.info(&format!("Pixel factory image: {url}"));
    let dest = work.join(url_filename(&url));
    // Cache key includes URL so a newer factory image invalidates.
    let fp = fingerprint::remote_firmware_digest("pixel", &[codename, &url]);
    if let Some(hit) = fingerprint::lookup_pristine(cache, &fp) {
        log.info(&format!(
            "firmware cache hit (pixel {}): {}",
            codename,
            hit.display()
        ));
        return Ok(Acquired {
            raw_super: hit,
            cache_hit: true,
        });
    }
    download_to(&url, &dest, log)?;
    let raw = unpack::to_raw_super(&dest, work, log)?;
    fingerprint::store_pristine(cache, &fp, &raw)?;
    Ok(Acquired {
        raw_super: raw,
        cache_hit: false,
    })
}

/// Stream a URL to a file with a progress bar (uses the shared ureq client).
fn download_to(url: &str, dest: &Path, log: &Logger) -> Result<()> {
    let head_len = ureq::head(url)
        .call()
        .ok()
        .and_then(|r| {
            r.header("Content-Length")
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(0);
    if head_len > 0 {
        if let Ok(meta) = std::fs::metadata(dest) {
            if meta.len() == head_len {
                log.detail(&format!(
                    "download already present ({})",
                    mmcore::human_bytes(head_len)
                ));
                return Ok(());
            }
        }
    }

    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let total = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(head_len);
    let mut reader = resp.into_reader();
    let mut w = std::io::BufWriter::new(
        std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?,
    );
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        done += n as u64;
        if total > 0 {
            log.op_progress(done, total);
        }
    }
    w.flush()?;
    Ok(())
}

/// Last path segment of a URL (query stripped); a sane default when empty.
fn url_filename(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let name = no_query.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() {
        "firmware.bin".to_string()
    } else {
        name.to_string()
    }
}

fn from_local(
    root: &Path,
    cfg: &BuildConfig,
    work: &Path,
    cache: &Path,
    log: &Logger,
) -> Result<Acquired> {
    if cfg.firmware.local_ap.trim().is_empty() {
        bail!(
            "firmware source needs a local file but [firmware].local_ap is empty\n  \
             Set it to an AP_*.tar(.md5), super.img.lz4, or super.img path, or change \
             [firmware].source to \"download\" / \"download-then-local\" and set \
             [device].model + [device].region.\n  Hint: run `mmbuild doctor`."
        );
    }
    let p = resolve(root, &cfg.firmware.local_ap);
    if !p.exists() {
        bail!(
            "local firmware not found: {}\n  \
             Fix [firmware].local_ap in build.toml, or place the AP/super image there.\n  \
             Hint: run `mmbuild doctor`.",
            p.display()
        );
    }
    let fp = fingerprint::local_firmware_digest(&p)?;
    if let Some(hit) = fingerprint::lookup_pristine(cache, &fp) {
        log.info(&format!(
            "firmware cache hit (local): {} → {}",
            p.display(),
            hit.display()
        ));
        return Ok(Acquired {
            raw_super: hit,
            cache_hit: true,
        });
    }
    log.info(&format!("using local firmware: {}", p.display()));
    unpack_and_cache(&p, work, cache, log)
}

fn from_fus(
    root: &Path,
    cfg: &BuildConfig,
    cache: &Path,
    work: &Path,
    log: &Logger,
) -> Result<Acquired> {
    let _ = root;
    if cfg.device.model.trim().is_empty() || cfg.device.region.trim().is_empty() {
        bail!(
            "FUS download needs [device].model and [device].region in build.toml \
             (e.g. model = \"SM-A125F\", region = \"XSG\")\n  \
             Or set [firmware].source = \"local\" with a valid [firmware].local_ap.\n  \
             Hint: run `mmbuild doctor`."
        );
    }
    std::fs::create_dir_all(cache).ok();

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

    let fp = fingerprint::remote_firmware_digest(
        "fus",
        &[
            &cfg.device.model,
            &cfg.device.region,
            &version,
            &info.filename,
            &info.size.to_string(),
        ],
    );
    if let Some(hit) = fingerprint::lookup_pristine(cache, &fp) {
        log.info(&format!(
            "firmware cache hit (FUS {}): {}",
            version,
            hit.display()
        ));
        return Ok(Acquired {
            raw_super: hit,
            cache_hit: true,
        });
    }

    client.binary_init(&info)?;

    let enc_path = cache.join(&info.filename);
    let remote = format!("{}{}", info.model_path, info.filename);
    {
        let mut cb = |c: u64, t: u64| log.op_progress(c, t);
        client.download(&remote, &enc_path, info.size, Some(&mut cb))?;
    }

    // Decrypt .enc4 / .enc2 (skip when a matching decrypted blob is already cached).
    let dec_path = cache.join(strip_enc(&info.filename));
    let dec_meta = cache.join(format!("{}.dec.fp", strip_enc(&info.filename)));
    let dec_fp = fingerprint::digest_lines(&[
        version.clone(),
        info.filename.clone(),
        info.size.to_string(),
    ]);
    let dec_fresh = fingerprint::read_stamp(&dec_meta).as_deref() == Some(dec_fp.as_str())
        && dec_path.is_file()
        && std::fs::metadata(&dec_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
    if dec_fresh {
        log.detail("decrypt cache hit; reusing decrypted firmware");
    } else {
        let key = if info.is_enc2() {
            crypt::v2_key(&cfg.device.region, &cfg.device.model, &version)
        } else {
            crypt::v4_key(&info.fw_version, &info.logic_value)?
        };
        log.detail("decrypting firmware");
        {
            let mut cb = |c: u64, t: u64| log.op_progress(c, t);
            crypt::decrypt_file(&enc_path, &dec_path, &key, Some(&mut cb))?;
        }
        fingerprint::write_stamp(&dec_meta, &dec_fp)?;
    }
    if !cfg.firmware.keep_download {
        std::fs::remove_file(&enc_path).ok();
    }
    let raw = unpack::to_raw_super(&dec_path, work, log)?;
    fingerprint::store_pristine(cache, &fp, &raw)?;
    Ok(Acquired {
        raw_super: raw,
        cache_hit: false,
    })
}

fn unpack_and_cache(input: &Path, work: &Path, cache: &Path, log: &Logger) -> Result<Acquired> {
    let fp = fingerprint::local_firmware_digest(input)?;
    if let Some(hit) = fingerprint::lookup_pristine(cache, &fp) {
        log.info(&format!("firmware cache hit: {}", hit.display()));
        return Ok(Acquired {
            raw_super: hit,
            cache_hit: true,
        });
    }
    let raw = unpack::to_raw_super(input, work, log)?;
    fingerprint::store_pristine(cache, &fp, &raw)?;
    Ok(Acquired {
        raw_super: raw,
        cache_hit: false,
    })
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
