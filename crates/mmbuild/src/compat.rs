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

//! `mmbuild compat`: probe a phone over mmadb, match it against the
//! device_compat profiles, and write a tailored build.toml (known) or an
//! unverified profile stub (unknown). `--regen` rebuilds devices.inc, no phone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const DC_DIR: &str = "src/device_compat";

/// A device profile as parsed from a `devices/**/*.h` header.
#[derive(Debug, Clone)]
pub struct CompatProfile {
    pub symbol: String,   // e.g. "mm_dev_sm_a125f"
    pub rel_path: String, // e.g. "devices/samsung/sm_a125f.h" (relative to DC_DIR)
    pub model: String,
    pub codename: String,
    pub brand: String,
    pub platform: String,
    pub mcpu: String,
    pub arch_triple: String,
    pub super_partition: String,
    pub verified: bool,
}

/// What a connected phone reported.
#[derive(Debug, Clone, Default)]
pub struct DeviceProbe {
    pub model: String,
    pub codename: String,
    pub brand: String,
    pub platform: String,
    pub abi: String,
    pub has_devtmpfs: bool,
    pub ab_slots: bool,
    pub dynamic_super: bool,
    pub connsys_wifi: bool,
    pub props: BTreeMap<String, String>,
}

fn dc_path(root: &Path) -> PathBuf {
    root.join(DC_DIR)
}

// ----------------------------- header parsing ----------------------------

/// Parse every device header under `devices/` (excluding `_*.h`).
pub fn parse_profiles(root: &Path) -> Result<Vec<CompatProfile>> {
    let devices_dir = dc_path(root).join("devices");
    let mut out = Vec::new();
    if !devices_dir.exists() {
        return Ok(out);
    }
    for entry in walkdir::WalkDir::new(&devices_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.ends_with(".h") || name.starts_with('_') {
            continue;
        }
        if let Some(p) = parse_header(path, &devices_dir, &dc_path(root))? {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

fn parse_header(path: &Path, _devices_dir: &Path, dc_root: &Path) -> Result<Option<CompatProfile>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;

    // Symbol: the identifier after `mm_device_profile_t`.
    let symbol = text
        .lines()
        .find(|l| l.contains("mm_device_profile_t") && l.contains("mm_dev_"))
        .and_then(|l| {
            l.split_whitespace()
                .find(|t| t.starts_with("mm_dev_"))
                .map(|t| t.trim_end_matches(['=', '{', ' ']).to_string())
        });
    let symbol = match symbol {
        Some(s) => s,
        None => return Ok(None), // not a profile header
    };

    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('.') {
            if let Some((k, v)) = rest.split_once('=') {
                let key = k.trim().to_string();
                let val = v.trim().trim_end_matches(',').trim();
                let val = val.trim_matches('"').to_string();
                fields.entry(key).or_insert(val);
            }
        }
    }

    let rel_path = path
        .strip_prefix(dc_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");

    let brand = fields
        .get("brand")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        });

    Ok(Some(CompatProfile {
        symbol,
        rel_path,
        model: fields.get("model").cloned().unwrap_or_default(),
        codename: fields.get("codename").cloned().unwrap_or_default(),
        brand,
        platform: fields.get("platform").cloned().unwrap_or_default(),
        mcpu: fields.get("mcpu").cloned().unwrap_or_default(),
        arch_triple: fields.get("arch_triple").cloned().unwrap_or_default(),
        super_partition: fields
            .get("super_partition")
            .cloned()
            .unwrap_or_else(|| "system".to_string()),
        verified: fields.get("verified").map(|v| v.trim() == "1").unwrap_or(false),
    }))
}

/// Best match for an identity (model > codename > platform).
pub fn match_profile<'a>(
    profiles: &'a [CompatProfile],
    model: &str,
    codename: &str,
    platform: &str,
) -> Option<&'a CompatProfile> {
    let ci = |a: &str, b: &str| !a.is_empty() && a.eq_ignore_ascii_case(b);
    let mut best: Option<(&CompatProfile, u8)> = None;
    for p in profiles {
        let score = if ci(&p.model, model) {
            3
        } else if ci(&p.codename, codename) {
            2
        } else if ci(&p.platform, platform) {
            1
        } else {
            0
        };
        if score > 0 && best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((p, score));
        }
    }
    best.map(|(p, _)| p)
}

// ----------------------------- devices.inc regen -------------------------

/// Rebuild `devices.inc` from the headers. Returns true if the file changed.
pub fn regen_inc(root: &Path) -> Result<bool> {
    let profiles = parse_profiles(root)?;
    let mut body = String::new();
    body.push_str(
        "/* Generated by `mmbuild compat --regen`; do not edit.\n\
         * Two-phase include: MM_DEV_DECLARE defines the objects, MM_DEV_LIST\n\
         * the registry array. '_'-prefixed headers are excluded. */\n",
    );
    body.push_str("#if defined(MM_DEV_DECLARE)\n");
    for p in &profiles {
        body.push_str(&format!("#  include \"{}\"\n", p.rel_path));
    }
    body.push_str("#elif defined(MM_DEV_LIST)\n");
    for p in &profiles {
        body.push_str(&format!("    &{},\n", p.symbol));
    }
    body.push_str("#endif\n");

    let path = dc_path(root).join("devices.inc");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if old == body {
        return Ok(false);
    }
    std::fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Pin the build-target model in `dc_target.h`.
pub fn write_target(root: &Path, model: &str) -> Result<()> {
    let body = format!(
        "/* Generated by mmbuild; do not edit or commit. Build-target model. */\n\
         #ifndef MM_DC_TARGET_H\n\
         #define MM_DC_TARGET_H\n\
         #define MM_TARGET_MODEL {model:?}\n\
         #endif\n",
    );
    let path = dc_path(root).join("dc_target.h");
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

// ----------------------------- phone probing -----------------------------

/// Connect to a phone and gather identity + capability probes.
pub fn probe_device() -> Result<DeviceProbe> {
    let mut dev = mmadb::AdbDevice::connect_first()?;

    let getprop = dev.shell("getprop").context("adb getprop")?;
    let props = parse_getprop(&getprop);

    let first = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(v) = props.get(*k) {
                if !v.is_empty() {
                    return v.clone();
                }
            }
        }
        String::new()
    };

    let model = first(&[
        "ro.product.system.model",
        "ro.product.model",
        "ro.product.vendor.model",
    ]);
    let codename = first(&[
        "ro.product.system.device",
        "ro.product.device",
        "ro.product.vendor.device",
        "ro.build.product",
    ]);
    let brand = first(&["ro.product.brand", "ro.product.manufacturer", "ro.product.system.brand"])
        .to_lowercase();
    let platform = first(&["ro.board.platform", "ro.hardware", "ro.chipname"]);
    let abi = first(&["ro.product.cpu.abi"]);
    let ab_slots = !first(&["ro.boot.slot_suffix"]).is_empty();
    let dynamic_super = first(&["ro.boot.dynamic_partitions"]) == "true";

    // Live capability probes (best-effort; absence is reported honestly).
    let mounts = dev.shell("mount").unwrap_or_default();
    let has_devtmpfs = mounts.contains("devtmpfs");
    let wmt = dev.shell("ls -d /dev/wmtWifi /proc/driver/wmt_user_proc 2>/dev/null").unwrap_or_default();
    let connsys_wifi = wmt.contains("wmt");

    Ok(DeviceProbe {
        model,
        codename,
        brand,
        platform,
        abi,
        has_devtmpfs,
        ab_slots,
        dynamic_super,
        connsys_wifi,
        props,
    })
}

fn parse_getprop(text: &str) -> BTreeMap<String, String> {
    // Lines look like: [ro.product.model]: [SM-A125F]
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once("]: [") {
            let key = k.trim_start_matches('[').trim();
            let val = v.trim_end_matches(']').trim();
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

// --------------------------- generation ----------------------------------

fn sanitize_symbol(model: &str) -> String {
    let mut s: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    format!("mm_dev_{}", s.trim_matches('_'))
}

fn arch_for_abi(abi: &str) -> &'static str {
    if abi.contains("arm64") || abi.contains("aarch64") {
        "aarch64-linux-musl"
    } else if abi.contains("armeabi") || abi.starts_with("arm") {
        "arm-linux-musleabi"
    } else if abi.contains("x86_64") {
        "x86_64-linux-musl"
    } else {
        "aarch64-linux-musl"
    }
}

fn mcpu_for_platform(platform: &str) -> &'static str {
    match platform {
        p if p.starts_with("mt6765") => "cortex_a53",
        p if p.starts_with("mt6768") => "cortex_a55",
        _ => "generic",
    }
}

/// Build the contents of an unverified stub profile header for an unknown phone.
pub fn gen_stub_header(probe: &DeviceProbe) -> (String, String) {
    let symbol = sanitize_symbol(&probe.model);
    let brand = if probe.brand.is_empty() { "unknown".to_string() } else { probe.brand.clone() };
    let rel = format!("devices/{}/{}.h", brand, symbol.trim_start_matches("mm_dev_"));
    let arch = arch_for_abi(&probe.abi);
    let mcpu = mcpu_for_platform(&probe.platform);
    let wifi = if probe.connsys_wifi { "MM_WIFI_MTK_CONNSYS" } else { "MM_WIFI_UNKNOWN" };
    let devtmpfs = if probe.has_devtmpfs { 1 } else { 0 };
    let ab = if probe.ab_slots { 1 } else { 0 };
    let dynsuper = if probe.dynamic_super { 1 } else { 0 };

    let body = format!(
        "/* SPDX-License-Identifier: GPL-3.0-or-later\n\
         *\n\
         * MicroMiner OS - device profile (AUTO-GENERATED by `mmbuild compat`).\n\
         * Copyright (C) 2026 Vakea\n\
         *\n\
         * UNVERIFIED: these fields were detected from build.prop + live probes and\n\
         * MUST be confirmed on the real hardware. Fields that could not be detected\n\
         * are left UNKNOWN. Set `verified = 1` only after you have checked them.\n\
         */\n\n\
         static const mm_device_profile_t {symbol} = {{\n\
         \x20   .model = {model:?},\n\
         \x20   .codename = {codename:?},\n\
         \x20   .brand = {brand:?},\n\
         \x20   .platform = {platform:?},\n\n\
         \x20   .soc = MM_SOC_UNKNOWN,       /* TODO: pick the MM_SOC_* for {platform} */\n\
         \x20   .arch_triple = {arch:?},\n\
         \x20   .mcpu = {mcpu:?},\n\n\
         \x20   .has_devtmpfs = {devtmpfs},   /* probed: {devtmpfs_note} */\n\
         \x20   .needs_apex_tmpfs = 0,  /* TODO: confirm (RO rootfs needs tmpfs on /apex) */\n\
         \x20   .ab_slots = {ab},\n\
         \x20   .dynamic_super = {dynsuper},\n\n\
         \x20   .pa = MM_PA_UNKNOWN,         /* TODO: identify the loudspeaker amplifier */\n\
         \x20   .pa_pcm_hint = NULL,\n\n\
         \x20   .wifi = {wifi},\n\
         \x20   .wifi_iface = \"wlan0\",\n\n\
         \x20   .fb_width = 0,\n\
         \x20   .fb_height = 0,\n\n\
         \x20   .super_partition = \"system\",\n\n\
         \x20   .verified = 0,\n\
         \x20   .notes = \"Auto-generated by mmbuild compat; verify before trusting.\",\n\
         }};\n",
        symbol = symbol,
        model = probe.model,
        codename = probe.codename,
        brand = brand,
        platform = probe.platform,
        arch = arch,
        mcpu = mcpu,
        devtmpfs = devtmpfs,
        devtmpfs_note = if probe.has_devtmpfs { "devtmpfs mounted on /dev" } else { "no devtmpfs (cold-boot /dev)" },
        ab = ab,
        dynsuper = dynsuper,
        wifi = wifi,
    );
    (rel, body)
}

/// Render a device-tailored build.toml, based on the existing root build.toml
/// when present (so it mirrors the proven config) with device fields overridden.
pub fn gen_build_toml(root: &Path, p: &CompatProfile, region: &str) -> String {
    // Keep this close to the hand-written build.toml so the output is familiar.
    let base = std::fs::read_to_string(root.join("build.toml")).ok();
    let _ = base; // currently we render a fresh, minimal file; base kept for future merge.
    format!(
        "# Generated by `mmbuild compat` for {model} ({brand}/{platform}).\n\
         # Fill in [firmware].local_ap with your stock AP tar / super image.\n\n\
         [device]\n\
         model = {model:?}\n\
         region = {region:?}\n\
         compat = \"soc-layout\"\n\n\
         [firmware]\n\
         source = \"local\"\n\
         local_ap = \"\"\n\
         keep_download = true\n\n\
         [source]\n\
         include = [\n\
         \x20   \"src/asm/**/*.S\",\n\
         \x20   \"src/common/**/*.c\",\n\
         \x20   \"src/init/**/*.c\",\n\
         \x20   \"src/net/**/*.c\",\n\
         \x20   \"src/miner/**/*.c\",\n\
         \x20   \"src/ota/**/*.c\",\n\
         \x20   \"src/device_compat/**/*.c\",\n\
         ]\n\
         exclude = [\"src/init/trampoline.c\"]\n\
         third_party = [\"qrcodegen\", \"tweetnacl\"]\n\
         include_dirs = [\"src\", \"src/asm\", \"src/common\", \"src/init\", \"src/miner\", \"src/net\", \"src/ota\", \"src/device_compat\"]\n\
         target = {target:?}\n\
         mcpu = {mcpu:?}\n\
         cflags = [\"-static\", \"-O3\", \"-Wall\", \"-Wextra\", \"-Werror\", \"-pthread\", \"-lm\"]\n\
         output = \"rootfs/init\"\n\n\
         [test]\n\
         include_dirs = [\"tests/shim\", \"src\", \"src/common\", \"src/init\", \"src/miner\", \"src/net\", \"src/ota\"]\n\n\
         [patch]\n\
         partition = {partition:?}\n\n\
         [[patch.replace]]\n\
         path = \"/system/bin/init\"\n\
         from = \"rootfs/init\"\n\n\
         [[patch.replace]]\n\
         path = \"/system/bin/mm_wpa_supplicant\"\n\
         from = \"rootfs/mm_wpa_supplicant\"\n\n\
         [[patch.replace]]\n\
         path = \"/system/bin/mmgui\"\n\
         from = \"rootfs/mmgui\"\n\n\
         [package]\n\
         tar_name = \"{{device}}_System_OS.tar\"\n\
         members = [\"super.img.lz4\"]\n",
        model = p.model,
        brand = p.brand,
        platform = p.platform,
        region = region,
        target = if p.arch_triple.is_empty() { "aarch64-linux-musl" } else { &p.arch_triple },
        mcpu = if p.mcpu.is_empty() { "cortex_a53" } else { &p.mcpu },
        partition = if p.super_partition.is_empty() { "system" } else { &p.super_partition },
    )
}

// ------------------------------- command ---------------------------------

/// `mmbuild compat [--regen]`.
pub fn run(root: &Path, regen_only: bool) -> Result<()> {
    if regen_only {
        let changed = regen_inc(root)?;
        let n = parse_profiles(root)?.len();
        println!(
            "compat: {} device profile(s); devices.inc {}",
            n,
            if changed { "regenerated" } else { "already up to date" }
        );
        return Ok(());
    }

    // Keep the registry in sync first.
    regen_inc(root)?;
    let profiles = parse_profiles(root)?;

    println!("compat: scanning USB for a connected phone (ADB)…");
    let probe = probe_device()?;
    println!(
        "compat: {} / {} (brand {}, platform {})",
        dash(&probe.model),
        dash(&probe.codename),
        dash(&probe.brand),
        dash(&probe.platform)
    );
    println!(
        "        probes: devtmpfs={}  a/b={}  dynamic-super={}  mtk-connsys-wifi={}  abi={}",
        probe.has_devtmpfs, probe.ab_slots, probe.dynamic_super, probe.connsys_wifi, dash(&probe.abi)
    );

    match match_profile(&profiles, &probe.model, &probe.codename, &probe.platform) {
        Some(p) => {
            let region = probe
                .props
                .get("ro.csc.sales_code")
                .or_else(|| probe.props.get("ro.boot.sales_code"))
                .cloned()
                .unwrap_or_default();
            let toml = gen_build_toml(root, p, &region);
            let (out, note) = if root.join("build.toml").exists() {
                (root.join(format!("build.{}.toml", safe_name(&p.model))), true)
            } else {
                (root.join("build.toml"), false)
            };
            std::fs::write(&out, toml).with_context(|| format!("write {}", out.display()))?;
            println!(
                "compat: matched profile {} ({}).",
                p.symbol,
                if p.verified { "verified" } else { "UNVERIFIED — confirm fields" }
            );
            println!("compat: wrote {}", out.display());
            if note {
                println!(
                    "        (build.toml already exists; wrote a device-specific copy — \
                     build it with `mmbuild build --config {}`)",
                    out.file_name().unwrap().to_string_lossy()
                );
            }
            println!("        set [firmware].local_ap to your stock AP file, then `mmbuild build`.");
        }
        None => {
            if probe.model.is_empty() {
                bail!("device did not report a model via build.prop; cannot generate a profile");
            }
            let (rel, body) = gen_stub_header(&probe);
            let path = dc_path(root).join(&rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if path.exists() {
                println!("compat: a profile already exists at {}; not overwriting", rel);
            } else {
                std::fs::write(&path, body)
                    .with_context(|| format!("write {}", path.display()))?;
                regen_inc(root)?;
                println!("compat: unknown device — wrote an UNVERIFIED profile stub:");
                println!("          {}/{}", DC_DIR, rel);
                println!("        Confirm the TODO fields on the hardware, set verified = 1,");
                println!("        then re-run `mmbuild compat` to generate its build.toml.");
            }
        }
    }
    Ok(())
}

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

fn safe_name(model: &str) -> String {
    model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getprop_parsing() {
        let t = "[ro.product.model]: [SM-A125F]\n[ro.board.platform]: [mt6765]\n[x]: []";
        let m = parse_getprop(t);
        assert_eq!(m.get("ro.product.model").unwrap(), "SM-A125F");
        assert_eq!(m.get("ro.board.platform").unwrap(), "mt6765");
        assert_eq!(m.get("x").unwrap(), "");
    }

    #[test]
    fn symbol_sanitization() {
        assert_eq!(sanitize_symbol("SM-A125F"), "mm_dev_sm_a125f");
        assert_eq!(sanitize_symbol("Pixel 4a (5G)"), "mm_dev_pixel_4a_5g");
    }

    #[test]
    fn arch_and_mcpu_maps() {
        assert_eq!(arch_for_abi("arm64-v8a"), "aarch64-linux-musl");
        assert_eq!(arch_for_abi("armeabi-v7a"), "arm-linux-musleabi");
        assert_eq!(mcpu_for_platform("mt6765"), "cortex_a53");
    }
}
