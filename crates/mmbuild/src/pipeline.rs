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

//! The end-to-end build pipeline, reported as Gradle-style tasks:
//!   acquireFirmware → readDeviceProps → verifyCompat → compileDevice →
//!   buildWeb → patchSystem → repackSuper → package.
//!
//! Nothing device-specific is hardcoded: the partition geometry comes from LP
//! metadata, the device name/SoC from build.prop, and the output tar name is
//! derived from that device name.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mmcore::log::{Logger, Outcome};
use superimg::ext4::Ext4;
use superimg::{lp, lz4frame, sparse, tarball};

use crate::buildprop::{self, DeviceProps};
use crate::config::{expand_device, BuildConfig, CompatPolicy, Settings};
use crate::{subproject, toolchain};

pub struct Built {
    pub tar: PathBuf,
    pub device: String,
}

/// Which partition images `mmbuild build` produces and co-packs into the final
/// Odin tar. `super` (the patched system OS) is always built; `recovery` and
/// `boot` are opt-in via `--target`.
#[derive(Clone, Copy, Default)]
pub struct BuildTargets {
    pub recovery: bool,
    pub boot: bool,
}

impl BuildTargets {
    /// Parse the repeatable `--target` values. An empty list means "super +
    /// recovery" — a plain `mmbuild build` always co-packs the recovery image so
    /// the flashable tar is self-contained. Pass explicit `-t` values to choose
    /// exactly what to build (e.g. `-t super` for system-only, `-t all` for
    /// everything). `super`/`system` are accepted no-ops since the system image
    /// is always built.
    pub fn parse(items: &[String]) -> Result<Self> {
        // No flags → the default package includes recovery.
        if items.iter().all(|s| s.trim().is_empty()) {
            return Ok(BuildTargets {
                recovery: true,
                boot: false,
            });
        }
        let mut t = BuildTargets::default();
        for it in items {
            match it.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "super" | "system" => {}
                "recovery" => t.recovery = true,
                "boot" => t.boot = true,
                "all" => {
                    t.recovery = true;
                    t.boot = true;
                }
                other => bail!(
                    "unknown build target '{other}' (expected: super, recovery, boot, all)"
                ),
            }
        }
        Ok(t)
    }
}

pub fn run(
    root: &Path,
    cfg: &BuildConfig,
    settings: &Settings,
    targets: BuildTargets,
    log: &Logger,
) -> Result<Built> {
    log.plan(8 + targets.recovery as u32 + targets.boot as u32);
    log.set_phase("EXECUTING");
    log.lifecycle(&format!(
        "MicroMiner build — {} jobs, firmware source {:?}",
        settings.effective_jobs(),
        cfg.firmware.source
    ));

    let work = resolve(root, &settings.paths.work);
    std::fs::create_dir_all(&work)?;

    // 1. Acquire firmware → raw super (copied into workdir so we never edit the
    //    user's source file in place).
    log.task_start("mmbuild:acquireFirmware");
    let acquired = crate::firmware::acquire_raw_super(root, cfg, settings, log)?;
    let raw_super = ensure_in_workdir(&acquired, &work)?;
    log.task_done("mmbuild:acquireFirmware", Outcome::Executed);

    // 2. Read device identity from the firmware's build.prop (no hardcoding).
    log.task_start("mmbuild:readDeviceProps");
    let meta = lp::read_metadata(&raw_super).context("read LP metadata of super")?;
    let part = meta
        .partition_or_slot_a(&cfg.patch.partition)
        .with_context(|| format!("partition '{}' not in super", cfg.patch.partition))?;
    let (poff, psize) = (
        part.first_offset().context("partition has no extent")?,
        part.size_bytes(),
    );
    let props = {
        let mut fs = Ext4::open(&raw_super, poff, psize, false)?;
        buildprop::read_from_ext4(&mut fs)?
    };
    let device = props.output_stem();
    log.info(&format!(
        "device: model={:?} codename={:?} soc={:?}",
        props.model(),
        props.device(),
        props.platform()
    ));
    log.task_done("mmbuild:readDeviceProps", Outcome::Executed);

    // 3. Compatibility verification.
    log.task_start("mmbuild:verifyCompat");
    verify_compat(cfg, &props, &meta, &raw_super, poff, psize, log)?;
    log.task_done("mmbuild:verifyCompat", Outcome::Executed);

    // 4. Build sub-projects (mmgui, recovery, external deps) and install their
    //    outputs in place (e.g. build/mmgui → rootfs/mmgui) so patchSystem below
    //    picks up freshly-built companion binaries. No tar is produced here.
    log.task_start("mmbuild:buildSubprojects");
    let subs = subproject::build_all(root, settings, log, None, false)?;
    log.task_done(
        "mmbuild:buildSubprojects",
        if subs.is_empty() { Outcome::Skipped } else { Outcome::Executed },
    );

    // 5. Cross-compile the device binary.
    log.task_start("mmbuild:compileDevice");
    toolchain::compile_device(root, cfg, settings, log)?;
    log.task_done("mmbuild:compileDevice", Outcome::Executed);

    // 6. Build the web portal.
    log.task_start("mmbuild:buildWeb");
    let web_out = toolchain::build_web(root, cfg, settings, log)?;
    log.task_done(
        "mmbuild:buildWeb",
        if web_out.is_some() { Outcome::Executed } else { Outcome::Skipped },
    );

    // 7. Patch the system partition (replace files + sync web tree).
    log.task_start("mmbuild:patchSystem");
    {
        let mut fs = Ext4::open(&raw_super, poff, psize, true)?;
        for r in &cfg.patch.replace {
            let src = resolve(root, &r.from);
            let data = std::fs::read(&src).with_context(|| format!("read {}", src.display()))?;
            if fs.lookup(&r.path).is_ok() {
                log.detail(&format!("replace {}", r.path));
                fs.replace_file(&r.path, &data)
                    .with_context(|| format!("replace {}", r.path))?;
                log.info(&format!(
                    "patched {} ({})",
                    r.path,
                    mmcore::human_bytes(data.len() as u64)
                ));
            } else {
                // New file not present in the stock image (e.g. our mm_hostapd):
                // create it in its parent directory and mark it executable, since
                // create_file_in defaults to 0644.
                let p = r.path.trim_end_matches('/');
                let (dir, name) = p.rsplit_once('/').unwrap_or(("", p));
                let dir = if dir.is_empty() { "/" } else { dir };
                let parent = fs
                    .lookup(dir)
                    .with_context(|| format!("parent dir {dir} for new file {}", r.path))?;
                fs.create_file_in(parent, name, &data, None)
                    .with_context(|| format!("create {}", r.path))?;
                fs.chmod_reg(&r.path, 0o755)
                    .with_context(|| format!("chmod {}", r.path))?;
                log.info(&format!(
                    "added {} ({}, mode 0755)",
                    r.path,
                    mmcore::human_bytes(data.len() as u64)
                ));
            }
        }
        if let Some(out) = &web_out {
            log.detail(&format!("sync {} ← web", cfg.web.dest));
            fs.sync_tree(&cfg.web.dest, out)
                .with_context(|| format!("sync web into {}", cfg.web.dest))?;
            log.info(&format!("synced web portal into {}", cfg.web.dest));
        }

        // Stage the runtime linker for our minimal init. The dynamic linker and
        // bionic libc live inside com.android.runtime.apex; stock apexd loop-
        // mounts it at boot, but our init doesn't run apexd — so WITHOUT this,
        // /apex/com.android.runtime/bin/linker64 doesn't exist and every
        // dynamically-linked vendor/system binary (wmt_loader, wmt_launcher,
        // hostapd, wpa_supplicant, rild, ...) fails to exec with rc=127.
        // We extract the apex's ext4 payload as a plain image so init can
        // loop-mount it; see mount_runtime_apex() in init/main.c.
        {
            let apex = "/system/apex/com.android.runtime.apex";
            let img = "/system/apex/com.android.runtime.img";
            match fs.read_file(apex) {
                Ok(bytes) => match extract_stored_zip_entry(&bytes, "apex_payload.img") {
                    Some(payload) => {
                        if fs.lookup(img).is_ok() {
                            fs.replace_file(img, payload)?;
                        } else {
                            let parent = fs.lookup("/system/apex")
                                .context("locate /system/apex to stage runtime image")?;
                            fs.create_file_in(parent, "com.android.runtime.img", payload, None)?;
                        }
                        log.info(&format!(
                            "staged runtime linker image {} ({})",
                            img,
                            mmcore::human_bytes(payload.len() as u64)
                        ));
                    }
                    None => log.info(
                        "warning: apex_payload.img not STORED in runtime apex; linker not staged",
                    ),
                },
                Err(_) => log.info(
                    "note: com.android.runtime.apex absent; skipping runtime linker stage",
                ),
            }
        }

        let free = fs.superblock().free_blocks_count * fs.superblock().block_size as u64;
        log.info(&format!("free space after patch: {}", mmcore::human_bytes(free)));
    }
    log.task_done("mmbuild:patchSystem", Outcome::Executed);

    // 7b. Optional extra target images. We build these BEFORE the tar is sealed
    //     so they ride in the SAME flashable {device}_System_OS.tar as the system
    //     image, each compressed to the lz4 member Odin expects.
    let mut extra: Vec<(String, PathBuf)> = Vec::new();
    if targets.recovery {
        // Our custom recovery (recovery_init + mmgui menu) ONLY. We deliberately
        // do NOT fold a vbmeta into the system tar: the device's existing vbmeta
        // works and must be kept as-is, so shipping ours here would needlessly
        // overwrite a known-good partition. (The standalone `mmbuild recovery`
        // _Recovery.tar under build/ still carries its patched vbmeta for the
        // separate unlocked-bootloader flashing path if it is ever needed.)
        log.task_start("mmbuild:buildRecovery");
        let art = crate::recovery::build(root, cfg, settings, &device, log)
            .context("build recovery target")?;
        let rlz4 = work.join("recovery.img.lz4");
        lz4frame::compress_file(&art.img, &rlz4, None)?;
        extra.push(("recovery.img.lz4".into(), rlz4));
        log.task_done("mmbuild:buildRecovery", Outcome::Executed);
    }
    if targets.boot {
        // Stock boot.img, kept exactly as shipped (we do not modify boot — see
        // the system-img approach). Included only so it can be reflashed from the
        // same tar "in case we need to"; extracted straight from the AP firmware.
        log.task_start("mmbuild:stageBoot");
        let ap = resolve(root, &cfg.firmware.local_ap);
        if !ap.exists() {
            bail!(
                "boot target needs the AP firmware ([firmware].local_ap); not found at {}",
                ap.display()
            );
        }
        let boot_raw = work.join("boot_stock.img");
        crate::recovery::extract_image(&ap, "boot", &boot_raw, log)
            .context("extract stock boot.img from AP")?;
        let blz4 = work.join("boot.img.lz4");
        lz4frame::compress_file(&boot_raw, &blz4, None)?;
        extra.push(("boot.img.lz4".into(), blz4));
        log.task_done("mmbuild:stageBoot", Outcome::Executed);
    }

    // 8. Repack: raw → sparse → lz4 → tar (super + any extra target images).
    log.task_start("mmbuild:repackSuper");
    let tar = repack(root, cfg, settings, &raw_super, &meta, &device, &work, &extra, log)?;
    log.task_done("mmbuild:repackSuper", Outcome::Executed);

    Ok(Built { tar, device })
}

fn verify_compat(
    cfg: &BuildConfig,
    props: &DeviceProps,
    meta: &lp::Metadata,
    raw_super: &Path,
    poff: u64,
    psize: u64,
    log: &Logger,
) -> Result<()> {
    // Layout: the partition we patch must be a contiguous ext4 we can open.
    let part = meta
        .partition_or_slot_a(&cfg.patch.partition)
        .context("target partition missing")?;
    if !part.is_contiguous() {
        bail!(
            "partition '{}' is non-contiguous; in-place patch needs a single extent",
            cfg.patch.partition
        );
    }
    Ext4::open(raw_super, poff, psize, false)
        .with_context(|| format!("partition '{}' is not valid ext4", cfg.patch.partition))?;

    let fw_model = props.model().unwrap_or("");
    let same_model = !cfg.device.model.is_empty()
        && fw_model.eq_ignore_ascii_case(&cfg.device.model);

    match cfg.device.compat {
        CompatPolicy::SocLayout => {
            let soc = props.platform().unwrap_or("");
            if soc.is_empty() {
                log.warn("could not read SoC/platform from build.prop; proceeding on layout check only");
            } else {
                log.info(&format!("SoC verified: {soc}"));
            }
            if !same_model && !cfg.device.model.is_empty() {
                log.warn(&format!(
                    "firmware model {fw_model} differs from requested {}; accepted on SoC+layout match",
                    cfg.device.model
                ));
            }
        }
        CompatPolicy::ModelPrefix => {
            if !cfg.device.model.is_empty() {
                let want = &cfg.device.model[..cfg.device.model.len().min(6)];
                if !fw_model.starts_with(want) {
                    bail!("model {fw_model} does not share prefix {want}");
                }
            }
        }
        CompatPolicy::Curated => {
            // Curated list validation would load compat.toml; for now require exact.
            if !same_model {
                bail!("curated policy: firmware model {fw_model} != requested {}", cfg.device.model);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn repack(
    root: &Path,
    cfg: &BuildConfig,
    _settings: &Settings,
    raw_super: &Path,
    meta: &lp::Metadata,
    device: &str,
    work: &Path,
    extra: &[(String, PathBuf)],
    log: &Logger,
) -> Result<PathBuf> {
    let sparse_path = work.join("super.img");
    let block = if meta.logical_block_size != 0 {
        meta.logical_block_size
    } else {
        4096
    };
    log.detail("img2simg (raw → sparse)");
    {
        let mut cb = |c: u64, t: u64| log.op_progress(c, t);
        sparse::raw_to_sparse(raw_super, &sparse_path, block, Some(&mut cb))?;
    }
    let lz4_path = work.join("super.img.lz4");
    log.detail("lz4 (--content-size -B6)");
    {
        let mut cb = |c: u64, t: u64| log.op_progress(c, t);
        lz4frame::compress_file(&sparse_path, &lz4_path, Some(&mut cb))?;
    }
    // Intermediate sparse no longer needed.
    std::fs::remove_file(&sparse_path).ok();

    let tar_name = expand_device(&cfg.package.tar_name, device);
    let tar_path = root.join(&tar_name);
    log.detail(&format!("tar (ustar) → {tar_name}"));
    let f = std::fs::File::create(&tar_path)?;
    let mut tb = tarball::TarBuilder::new(std::io::BufWriter::new(f));
    for m in &cfg.package.members {
        let src = match m.as_str() {
            "super.img.lz4" => lz4_path.clone(),
            other => work.join(other),
        };
        if !src.exists() {
            bail!("tar member {m} not found at {}", src.display());
        }
        tb.append_path(m, &src)?;
    }
    // Co-packed target images (recovery.img.lz4, vbmeta.img.lz4, boot.img.lz4).
    for (name, src) in extra {
        if !src.exists() {
            bail!("target member {name} not found at {}", src.display());
        }
        tb.append_path(name, src)?;
        log.info(&format!("+ co-packed {name}"));
    }
    tb.finish()?;
    let sz = std::fs::metadata(&tar_path)?.len();
    log.lifecycle(&format!(
        "package: {} ({})",
        tar_path.display(),
        mmcore::human_bytes(sz)
    ));
    Ok(tar_path)
}

/// Copy the raw super into the workdir if it isn't already there, so we never
/// mutate the user's firmware file in place.
fn ensure_in_workdir(raw: &Path, work: &Path) -> Result<PathBuf> {
    let in_work = raw.parent().map(|p| p == work).unwrap_or(false);
    if in_work {
        return Ok(raw.to_path_buf());
    }
    let dest = work.join("super.raw");
    if dest.exists() && std::fs::metadata(&dest)?.len() == std::fs::metadata(raw)?.len() {
        return Ok(dest);
    }
    std::fs::copy(raw, &dest).with_context(|| format!("stage {} → {}", raw.display(), dest.display()))?;
    Ok(dest)
}

fn resolve(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}

/// Return the bytes of a STORED (uncompressed) entry `name` inside a ZIP archive
/// without inflating anything. APEX files are ZIPs whose `apex_payload.img` entry
/// is always stored uncompressed, so this slices it out directly. Returns None if
/// the entry is missing, compressed, or the archive is malformed.
fn extract_stored_zip_entry<'a>(zip: &'a [u8], name: &str) -> Option<&'a [u8]> {
    let le16 = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) as usize;
    let le32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;

    // Locate the End Of Central Directory record (sig 0x06054b50) from the tail.
    let eocd_sig = [0x50, 0x4b, 0x05, 0x06];
    let scan_start = zip.len().saturating_sub(65_557);
    let mut eocd = None;
    let mut i = zip.len().saturating_sub(22);
    while i >= scan_start {
        if zip[i..i + 4] == eocd_sig {
            eocd = Some(i);
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    let e = eocd?;
    let cd_off = le32(&zip[e + 16..e + 20]);
    let cd_size = le32(&zip[e + 12..e + 16]);
    let cd_end = (cd_off + cd_size).min(zip.len());

    // Walk central-directory entries (sig 0x02014b50) for a filename match.
    let mut p = cd_off;
    while p + 46 <= cd_end {
        if zip[p..p + 4] != [0x50, 0x4b, 0x01, 0x02] {
            break;
        }
        let method = le16(&zip[p + 10..p + 12]);
        let comp_size = le32(&zip[p + 20..p + 24]);
        let name_len = le16(&zip[p + 28..p + 30]);
        let extra_len = le16(&zip[p + 30..p + 32]);
        let comment_len = le16(&zip[p + 32..p + 34]);
        let local_off = le32(&zip[p + 42..p + 46]);
        let fname = &zip[p + 46..(p + 46 + name_len).min(zip.len())];
        if fname == name.as_bytes() {
            if method != 0 {
                return None; // not STORED; we don't inflate
            }
            if local_off + 30 > zip.len() || zip[local_off..local_off + 4] != [0x50, 0x4b, 0x03, 0x04] {
                return None;
            }
            let l_name = le16(&zip[local_off + 26..local_off + 28]);
            let l_extra = le16(&zip[local_off + 28..local_off + 30]);
            let data = local_off + 30 + l_name + l_extra;
            if data + comp_size > zip.len() {
                return None;
            }
            return Some(&zip[data..data + comp_size]);
        }
        p += 46 + name_len + extra_len + comment_len;
    }
    None
}
