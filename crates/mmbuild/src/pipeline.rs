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

//! Build pipeline: acquire firmware through subprojects, compile, patch system, repack super.

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

/// Which partition images `mmbuild build` produces and co-packs into the final Odin tar.
#[derive(Clone, Copy, Default)]
pub struct BuildTargets {
    pub recovery: bool,
    pub boot: bool,
}

impl BuildTargets {
    /// Parse the repeatable `--target` values.
    ///
    /// Empty list -> super only (build.toml `[package].members`). `recovery`,
    /// `boot` and `all` add the experimental custom images plus a patched
    /// vbmeta; the supported path is the soft Micro BIOS in `/system/bin/init`.
    pub fn parse(items: &[String]) -> Result<Self> {
        if items.iter().all(|s| s.trim().is_empty()) {
            return Ok(BuildTargets {
                recovery: false,
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
                other => {
                    bail!("unknown build target '{other}' (expected: super, recovery, boot, all)")
                }
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
    // Base tasks: acquire, props, compat, subprojects, compile, patch, repack.
    log.plan(7 + targets.recovery as u32 + targets.boot as u32);
    log.set_phase("EXECUTING");
    log.lifecycle(&format!(
        "MicroMiner build — {} jobs, firmware source {:?}",
        settings.effective_jobs(),
        cfg.firmware.source
    ));
    let work = resolve(root, &settings.paths.work);
    std::fs::create_dir_all(&work)?;

    // 1. Acquire firmware -> raw super (copied into workdir so we never edit the user's source file
    // / pristine cache in place). Cache hits still refresh work/super.raw because patchSystem
    // mutates it.
    log.task_start("mmbuild:acquireFirmware");
    let acquired = crate::firmware::acquire_raw_super(root, cfg, settings, log)?;
    let raw_super = ensure_in_workdir(&acquired.raw_super, &work)?;
    log.task_done(
        "mmbuild:acquireFirmware",
        if acquired.cache_hit {
            Outcome::UpToDate
        } else {
            Outcome::Executed
        },
    );

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

    // 4. Build sub-projects (mmgui, recovery, external deps) and install their outputs in place
    // (e.g. build/mmgui -> rootfs/mmgui) so patchSystem below picks up freshly-built companion
    // binaries. No tar is produced here.
    log.task_start("mmbuild:buildSubprojects");
    let subs = subproject::build_all(root, settings, log, None, false, &cfg.device.model)?;
    log.task_done(
        "mmbuild:buildSubprojects",
        if subs.is_empty() {
            Outcome::Skipped
        } else {
            Outcome::Executed
        },
    );

    // 5. Cross-compile the device binary (skipped when zig inputs are unchanged).
    log.task_start("mmbuild:compileDevice");
    let compiled = toolchain::compile_device(root, cfg, settings, log)?;
    log.task_done(
        "mmbuild:compileDevice",
        if compiled.up_to_date {
            Outcome::UpToDate
        } else {
            Outcome::Executed
        },
    );

    // 6. Patch the system partition (in-place file replacements + runtime apex).
    log.task_start("mmbuild:patchSystem");
    {
        let mut fs = Ext4::open(&raw_super, poff, psize, true)?;
        for r in &cfg.patch.replace {
            let src = resolve(root, &r.from);
            if !src.exists() {
                bail!(
                    "patch source missing for {}: {}\n  \
                     Build the binary first (compileDevice / `mmbuild sub`), or fix \
                     [[patch.replace]] in build.toml.\n  Hint: run `mmbuild doctor`.",
                    r.path,
                    src.display()
                );
            }
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
                // New file not present in the stock image (e.g. our mm_hostapd): create it in its
                // parent directory and mark it executable, since create_file_in defaults to 0644.
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

        // Stage the runtime linker for our minimal init. The dynamic linker and bionic libc live
        // inside com.android.runtime.apex; stock apexd loop- mounts it at boot, but our init
        // doesn't run apexd - so WITHOUT this, /apex/com.android.runtime/bin/linker64 doesn't exist
        // and every dynamically-linked vendor/system binary (wmt_loader, wmt_launcher, hostapd,
        // wpa_supplicant, rild, ...) fails to exec with rc=127. We extract the apex's ext4 payload
        // as a plain image so init can loop-mount it; see mount_runtime_apex() in init/main.c.
        {
            let apex = "/system/apex/com.android.runtime.apex";
            let img = "/system/apex/com.android.runtime.img";
            match fs.read_file(apex) {
                Ok(bytes) => match extract_stored_zip_entry(&bytes, "apex_payload.img") {
                    Some(payload) => {
                        if fs.lookup(img).is_ok() {
                            fs.replace_file(img, payload)?;
                        } else {
                            let parent = fs
                                .lookup("/system/apex")
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
                Err(_) => {
                    log.info("note: com.android.runtime.apex absent; skipping runtime linker stage")
                }
            }
        }

        let free = fs.superblock().free_blocks_count * fs.superblock().block_size as u64;
        log.info(&format!(
            "free space after patch: {}",
            mmcore::human_bytes(free)
        ));
    }
    log.task_done("mmbuild:patchSystem", Outcome::Executed);

    // Optional extra target images. Built BEFORE the tar is sealed so they ride
    // in the SAME flashable {device}_System_OS.tar as the system image.
    // Custom boot/recovery drop the stock AVB footer, so a verification-disabled
    // vbmeta MUST be co-packed or the bootloader rejects the images.
    let mut extra: Vec<(String, PathBuf)> = Vec::new();
    let mut vbmetas_for_tar: Vec<(String, PathBuf)> = Vec::new();
    if targets.recovery {
        log.info(
            "custom recovery: Magisk-style inject (keeps SEANDROID/SignerVer02, drops AVBf); \
             flash patched vbmeta",
        );
        log.task_start("mmbuild:buildRecovery");
        let art = crate::recovery::build(root, cfg, settings, &device, log)
            .context("build recovery target")?;
        let rlz4 = work.join("recovery.img.lz4");
        lz4frame::compress_file(&art.img, &rlz4, None)?;
        extra.push(("recovery.img.lz4".into(), rlz4));
        if !art.vbmetas.is_empty() {
            vbmetas_for_tar = art.vbmetas;
        }
        log.task_done("mmbuild:buildRecovery", Outcome::Executed);
    }
    if targets.boot {
        log.info(
            "boot: stock boot.img verbatim + patched vbmeta (soft Micro BIOS in /system/bin/init)",
        );
        log.task_start("mmbuild:buildBoot");
        let art = crate::boot::build_stock(root, cfg, settings, &device, log)
            .context("build boot target")?;
        let blz4 = work.join("boot.img.lz4");
        lz4frame::compress_file(&art.img, &blz4, None)?;
        extra.push(("boot.img.lz4".into(), blz4));
        if !art.vbmetas.is_empty() {
            vbmetas_for_tar = art.vbmetas;
        }
        log.task_done("mmbuild:buildBoot", Outcome::Executed);
    }
    if vbmetas_for_tar.is_empty() && (targets.boot || targets.recovery) {
        log.warn(
            "boot/recovery co-packed without vbmeta — device may refuse to boot; \
             flash vbmeta_disabled.img separately or unlock + fastboot --disable-verification",
        );
    }
    for (stem_img, vb) in vbmetas_for_tar {
        // stem_img is "vbmeta.img" / "vbmeta_samsung.img"
        let member = format!("{stem_img}.lz4");
        let vlz4 = work.join(&member);
        lz4frame::compress_file(&vb, &vlz4, None)?;
        extra.push((member.clone(), vlz4));
        log.info(&format!("co-packing {member} (AVB flags disabled)"));
    }

    // Repack: raw -> sparse -> lz4 -> tar (super + any extra target images).
    log.task_start("mmbuild:repackSuper");
    let tar = repack(
        root, cfg, settings, &raw_super, &meta, &device, &work, &extra, log,
    )?;
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
    let same_model =
        !cfg.device.model.is_empty() && fw_model.eq_ignore_ascii_case(&cfg.device.model);

    match cfg.device.compat {
        CompatPolicy::SocLayout => {
            let soc = props.platform().unwrap_or("");
            if soc.is_empty() {
                log.warn(
                    "could not read SoC/platform from build.prop; proceeding on layout check only",
                );
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
            // Exact match only; there is no curated compat list.
            if !same_model {
                bail!(
                    "curated policy: firmware model {fw_model} != requested {}",
                    cfg.device.model
                );
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
    // Drop the intermediate sparse image.
    std::fs::remove_file(&sparse_path).ok();

    let tar_name = expand_device(&cfg.package.tar_name, device);
    let tar_path = root.join(&tar_name);
    /* Write via a sibling temp file so Odin/7-Zip holding the previous tar
     * open doesn't fail the whole build (Windows ERROR_SHARING_VIOLATION). */
    let tar_tmp = root.join(format!("{tar_name}.tmp"));
    log.detail(&format!("tar (ustar) → {tar_name}"));
    {
        let f = std::fs::File::create(&tar_tmp)
            .with_context(|| format!("create {}", tar_tmp.display()))?;
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
        // Co-packed target images (recovery.img.lz4, boot.img.lz4).
        for (name, src) in extra {
            if !src.exists() {
                bail!("target member {name} not found at {}", src.display());
            }
            tb.append_path(name, src)?;
            log.info(&format!("+ co-packed {name}"));
        }
        tb.finish()?;
    }
    match std::fs::rename(&tar_tmp, &tar_path) {
        Ok(()) => {}
        Err(e) => {
            // Prefer a free sibling name when Odin/Explorer holds the usual
            // targets open (Windows ERROR_SHARING_VIOLATION / Access denied).
            let stem = tar_name.trim_end_matches(".tar");
            let alts = [
                root.join(format!("{stem}.new.tar")),
                root.join(format!("{stem}_flash.tar")),
                root.join(format!("{stem}.{}.tar", std::process::id())),
            ];
            let mut kept = None;
            let mut last_err = None;
            for alt in &alts {
                match std::fs::rename(&tar_tmp, alt) {
                    Ok(()) => {
                        kept = Some(alt.clone());
                        break;
                    }
                    Err(err) => last_err = Some(err),
                }
            }
            let alt = kept.with_context(|| {
                format!(
                    "replace {} failed ({e}); also failed to keep alternate ({})",
                    tar_path.display(),
                    last_err
                        .map(|err| err.to_string())
                        .unwrap_or_else(|| "no candidates".into())
                )
            })?;
            log.warn(&format!(
                "{} is locked (close Odin/7-Zip); wrote {}",
                tar_path.display(),
                alt.display()
            ));
            let sz = std::fs::metadata(&alt)?.len();
            log.lifecycle(&format!(
                "package: {} ({})",
                alt.display(),
                mmcore::human_bytes(sz)
            ));
            return Ok(alt);
        }
    }
    let sz = std::fs::metadata(&tar_path)?.len();
    log.lifecycle(&format!(
        "package: {} ({})",
        tar_path.display(),
        mmcore::human_bytes(sz)
    ));
    Ok(tar_path)
}

/// Stage a pristine raw super into the workdir.
fn ensure_in_workdir(raw: &Path, work: &Path) -> Result<PathBuf> {
    let dest = work.join("super.raw");
    let in_work = raw
        .canonicalize()
        .ok()
        .and_then(|r| dest.canonicalize().ok().map(|d| r == d))
        .unwrap_or(false);
    if in_work {
        return Ok(dest);
    }
    std::fs::copy(raw, &dest)
        .with_context(|| format!("stage {} → {}", raw.display(), dest.display()))?;
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

/// Return the bytes of a STORED (uncompressed) entry `name` inside a ZIP archive without inflating
/// anything.
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
            if local_off + 30 > zip.len()
                || zip[local_off..local_off + 4] != [0x50, 0x4b, 0x03, 0x04]
            {
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
