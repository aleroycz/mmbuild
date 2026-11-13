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

//! `mmbuild compat`: mmadb probe, device profile match, generate build.toml.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const DC_DIR: &str = "src/device_compat";

/// Fields parsed from a `devices/**/*.h` profile header.
#[derive(Debug, Clone)]
pub struct CompatProfile {
    pub rel_path: String,
    pub symbol: String,
    pub model: String,
    pub codename: String,
    pub brand: String,
    pub platform: String,
    pub arch_triple: String,
    pub mcpu: String,
    pub super_partition: String,
    pub verified: bool,
}

/// Live identity and capability probes from a connected phone (ADB).
#[derive(Debug, Clone)]
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

fn parse_profile_file(rel: &str, text: &str) -> Option<CompatProfile> {
    let symbol = text
        .lines()
        .find_map(|l| {
            let l = l.trim();
            l.strip_prefix("static const mm_device_profile_t ")
                .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        })
        .filter(|s| !s.is_empty())?;
    Some(CompatProfile {
        rel_path: rel.replace('\\', "/"),
        symbol,
        model: field_quoted(text, "model"),
        codename: field_quoted(text, "codename"),
        brand: field_quoted(text, "brand"),
        platform: field_quoted(text, "platform"),
        arch_triple: field_quoted(text, "arch_triple"),
        mcpu: field_quoted(text, "mcpu"),
        super_partition: field_quoted(text, "super_partition"),
        verified: field_bool(text, "verified"),
    })
}

fn field_quoted(text: &str, field: &str) -> String {
    let needle = format!(".{field} =");
    for line in text.lines() {
        let t = line.trim();
        if !t.contains(&needle) {
            continue;
        }
        if let Some(rest) = t.split_once(&needle) {
            let rest = rest.1.trim();
            if let Some(q) = rest.strip_prefix('"') {
                if let Some(end) = q.find('"') {
                    return q[..end].to_string();
                }
            }
        }
    }
    String::new()
}

fn field_bool(text: &str, field: &str) -> bool {
    let needle = format!(".{field} =");
    for line in text.lines() {
        let t = line.trim();
        if t.contains(&needle) {
            let rest = t.split_once(&needle).map(|x| x.1.trim()).unwrap_or("");
            return rest.starts_with('1');
        }
    }
    false
}

/// Load every `devices/**/*.h` profile (skips `_`-prefixed templates).
pub fn parse_profiles(root: &Path) -> Result<Vec<CompatProfile>> {
    use walkdir::WalkDir;
    let devices = dc_path(root).join("devices");
    let mut profiles = Vec::new();
    for entry in WalkDir::new(&devices)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let name = entry.file_name().to_string_lossy();
        if !name.ends_with(".h") || name.starts_with('_') {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(dc_path(root))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if let Some(p) = parse_profile_file(&rel, &text) {
            profiles.push(p);
        }
    }
    profiles.sort_by(|a, b| a.model.cmp(&b.model));
    Ok(profiles)
}

/// Pick a profile by model, then codename, then platform (case-insensitive).
pub fn match_profile<'a>(
    profiles: &'a [CompatProfile],
    model: &str,
    codename: &str,
    platform: &str,
) -> Option<&'a CompatProfile> {
    let eq = |a: &str, b: &str| !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b);
    profiles
        .iter()
        .find(|p| eq(&p.model, model))
        .or_else(|| profiles.iter().find(|p| eq(&p.codename, codename)))
        .or_else(|| profiles.iter().find(|p| eq(&p.platform, platform)))
}

/// Align a build.toml field with the matched profile when the profile is authoritative.
pub fn reconcile_field(
    name: &str,
    current: &mut String,
    profile_val: &str,
    model: &str,
    notes: &mut Vec<String>,
) -> Result<()> {
    if profile_val.is_empty() {
        return Ok(());
    }
    if current.is_empty() {
        *current = profile_val.to_string();
        notes.push(format!(
            "filled {name} from device_compat profile for {model}"
        ));
        return Ok(());
    }
    if current.eq_ignore_ascii_case(profile_val) {
        return Ok(());
    }
    bail!(
        "build.toml {name} ({current}) disagrees with device_compat profile for {model} ({profile_val})"
    );
}

/// Reconcile compile target fields in build.toml with a matched profile.
pub fn reconcile_target(
    root: &Path,
    model: &str,
    target: &mut String,
    mcpu: &mut String,
) -> Result<Vec<String>> {
    if model.is_empty() {
        return Ok(Vec::new());
    }
    let profiles = parse_profiles(root)?;
    let profile = match match_profile(&profiles, model, "", "") {
        Some(p) => p,
        None => {
            let dir = dc_path(root).join("devices");
            // Soft warning: the flashable pipeline can still run with build.toml's target/mcpu;
            // on-device BIOS will refuse unverified hardware. Doctor treats a missing profile as a
            // failure.
            return Ok(vec![format!(
                "no device_compat profile matched model {model:?} \
                 (looked under {}, {} parsed) — add a header or run `mmbuild compat`; \
                 compile target/mcpu stay as in build.toml",
                dir.display(),
                profiles.len()
            )]);
        }
    };
    let mut notes = Vec::new();
    reconcile_field(
        "target",
        target,
        &profile.arch_triple,
        &profile.model,
        &mut notes,
    )?;
    reconcile_field("mcpu", mcpu, &profile.mcpu, &profile.model, &mut notes)?;
    Ok(notes)
}

// ----------------------------- devices.inc regen -------------------------

/// Rebuild `devices.inc` from the headers. Returns true if the file changed.
pub fn regen_inc(root: &Path) -> Result<bool> {
    let profiles = parse_profiles(root)?;
    let mut body = String::new();
    body.push_str("/* Generated by `mmbuild compat --regen`; */\n");
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
    let brand = first(&[
        "ro.product.brand",
        "ro.product.manufacturer",
        "ro.product.system.brand",
    ])
    .to_lowercase();
    let platform = first(&["ro.board.platform", "ro.hardware", "ro.chipname"]);
    let abi = first(&["ro.product.cpu.abi"]);
    let ab_slots = !first(&["ro.boot.slot_suffix"]).is_empty();
    let dynamic_super = first(&["ro.boot.dynamic_partitions"]) == "true";

    // Live capability probes (best-effort; a missing node is reported as absent).
    let mounts = dev.shell("mount").unwrap_or_default();
    let has_devtmpfs = mounts.contains("devtmpfs");
    let wmt = dev
        .shell("ls -d /dev/wmtWifi /proc/driver/wmt_user_proc 2>/dev/null")
        .unwrap_or_default();
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
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
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
    let brand = if probe.brand.is_empty() {
        "unknown".to_string()
    } else {
        probe.brand.clone()
    };
    let rel = format!(
        "devices/{}/{}.h",
        brand,
        symbol.trim_start_matches("mm_dev_")
    );
    let arch = arch_for_abi(&probe.abi);
    let mcpu = mcpu_for_platform(&probe.platform);
    let wifi = if probe.connsys_wifi {
        "MM_WIFI_MTK_CONNSYS"
    } else {
        "MM_WIFI_UNKNOWN"
    };
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
         \x20   .has_cell = 0,           /* TODO: set 1 if modem / WWAN present */\n\n\
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
        devtmpfs_note = if probe.has_devtmpfs {
            "devtmpfs mounted on /dev"
        } else {
            "no devtmpfs (cold-boot /dev)"
        },
        ab = ab,
        dynsuper = dynsuper,
        wifi = wifi,
    );
    (rel, body)
}

/// Render build.toml for matched device profile.
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
         \x20   \"src/ipc/**/*.c\",\n\
         \x20   \"src/log/**/*.c\",\n\
         \x20   \"src/platform/**/*.c\",\n\
         \x20   \"src/storage/**/*.c\",\n\
         \x20   \"src/security/**/*.c\",\n\
         \x20   \"src/net/**/*.c\",\n\
         \x20   \"src/miner/**/*.c\",\n\
         \x20   \"src/ota/**/*.c\",\n\
         \x20   \"src/device_compat/**/*.c\",\n\
         \x20   \"src/third_party/**/*.c\",\n\
         ]\n\
         exclude = [\"src/init/trampoline.c\"]\n\
         third_party = [\"qrcodegen\", \"tweetnacl\", \"verushash\"]\n\
         include_dirs = [\"src\", \"src/third_party\"]\n\
         target = {target:?}\n\
         mcpu = {mcpu:?}\n\
         cflags = [\"-static\", \"-O3\", \"-Wall\", \"-Wextra\", \"-Werror\", \"-pthread\", \"-lm\"]\n\
         output = \"rootfs/init\"\n\n\
         [test]\n\
         include_dirs = [\"tests/shim\", \"src\", \"src/third_party\"]\n\n\
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
pub fn run(root: &Path, regen_only: bool, wait: bool) -> Result<()> {
    if regen_only {
        let changed = regen_inc(root)?;
        let n = parse_profiles(root)?.len();
        println!(
            "compat: {} device profile(s); devices.inc {}",
            n,
            if changed {
                "regenerated"
            } else {
                "already up to date"
            }
        );
        return Ok(());
    }

    // Keep the registry in sync first.
    regen_inc(root)?;
    let profiles = parse_profiles(root)?;

    if wait {
        // Up to 10 min, like `adb wait-for-device` but bounded for CMake/CI.
        mmadb::wait_for_device(Some(std::time::Duration::from_secs(600)))?;
    }
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
        probe.has_devtmpfs,
        probe.ab_slots,
        probe.dynamic_super,
        probe.connsys_wifi,
        dash(&probe.abi)
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
                (
                    root.join(format!("build.{}.toml", safe_name(&p.model))),
                    true,
                )
            } else {
                (root.join("build.toml"), false)
            };
            std::fs::write(&out, toml).with_context(|| format!("write {}", out.display()))?;
            println!(
                "compat: matched profile {} ({}).",
                p.symbol,
                if p.verified {
                    "verified"
                } else {
                    "UNVERIFIED — confirm fields"
                }
            );
            println!("compat: wrote {}", out.display());
            if note {
                println!(
                    "        (build.toml already exists; wrote a device-specific copy — \
                     build it with `mmbuild build --config {}`)",
                    out.file_name().unwrap().to_string_lossy()
                );
            }
            println!(
                "        set [firmware].local_ap to your stock AP file, then `mmbuild build`."
            );
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
                println!(
                    "compat: a profile already exists at {}; not overwriting",
                    rel
                );
            } else {
                std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
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

    #[test]
    fn reconcile_fills_unset_field_from_profile() {
        let mut cur = String::new();
        let mut notes = Vec::new();
        reconcile_field(
            "target",
            &mut cur,
            "aarch64-linux-musl",
            "SM-A125F",
            &mut notes,
        )
        .unwrap();
        assert_eq!(cur, "aarch64-linux-musl");
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn reconcile_accepts_matching_field_case_insensitively() {
        let mut cur = "AArch64-Linux-Musl".to_string();
        let mut notes = Vec::new();
        reconcile_field(
            "target",
            &mut cur,
            "aarch64-linux-musl",
            "SM-A125F",
            &mut notes,
        )
        .unwrap();
        assert_eq!(cur, "AArch64-Linux-Musl"); // unchanged, no drift
        assert!(notes.is_empty());
    }

    #[test]
    fn reconcile_rejects_drift() {
        let mut cur = "x86_64-linux-musl".to_string();
        let mut notes = Vec::new();
        let err = reconcile_field(
            "target",
            &mut cur,
            "aarch64-linux-musl",
            "SM-A125F",
            &mut notes,
        );
        assert!(err.is_err(), "mismatched target must fail the build");
    }

    #[test]
    fn reconcile_keeps_buildtoml_when_profile_unset() {
        let mut cur = "cortex_a55".to_string();
        let mut notes = Vec::new();
        reconcile_field("mcpu", &mut cur, "", "SM-A125F", &mut notes).unwrap();
        assert_eq!(cur, "cortex_a55"); // profile silent -> build.toml wins
        assert!(notes.is_empty());
    }
}
