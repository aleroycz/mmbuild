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

//! Interactive `init` wizard and clean/doctor utilities.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{BuildConfig, CompatPolicy, FirmwareSource, Settings};

/// Prompt helper: print `label [default]: `, return the typed line or default.
fn ask(prompt: &str, default: &str) -> Result<String> {
    let mut out = std::io::stdout().lock();
    if default.is_empty() {
        write!(out, "{prompt}: ")?;
    } else {
        write!(out, "{prompt} [{default}]: ")?;
    }
    out.flush()?;
    drop(out);
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line.to_string()
    })
}

fn ask_bool(prompt: &str, default: bool) -> Result<bool> {
    let d = if default { "Y/n" } else { "y/N" };
    let ans = ask(prompt, d)?;
    Ok(match ans.to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    })
}

fn ask_choice(prompt: &str, options: &[(&str, &str)], default_ix: usize) -> Result<usize> {
    println!("{prompt}");
    for (i, (name, desc)) in options.iter().enumerate() {
        let mark = if i == default_ix { "*" } else { " " };
        println!("  {mark} {}) {name} — {desc}", i + 1);
    }
    let def = (default_ix + 1).to_string();
    loop {
        let ans = ask("select", &def)?;
        if let Ok(n) = ans.parse::<usize>() {
            if n >= 1 && n <= options.len() {
                return Ok(n - 1);
            }
        }
        println!("  please enter 1..{}", options.len());
    }
}

/// Run the `init` wizard, writing build.toml (and settings.toml if absent).
pub fn init(root: &Path, build_path: &Path, settings_path: &Path, force: bool) -> Result<()> {
    if build_path.exists() && !force {
        let ow = ask_bool(
            &format!("{} exists — overwrite?", build_path.display()),
            false,
        )?;
        if !ow {
            println!("keeping existing {}", build_path.display());
            return Ok(());
        }
    }

    println!("\n== MicroMiner OS builder — project setup ==\n");
    println!("Device identity is NOT hardcoded: at build time the real model/SoC");
    println!("are read from the firmware's build.prop. Here you only choose which");
    println!("firmware to fetch and where your sources are.\n");

    let mut cfg = BuildConfig::template();

    // ---- firmware source ----
    let src_ix = ask_choice(
        "Firmware source",
        &[
            (
                "download+local",
                "try Samsung FUS, fall back to a local file",
            ),
            ("download", "Samsung FUS only"),
            ("local", "a local AP tar / super.img only"),
        ],
        0,
    )?;
    cfg.firmware.source = match src_ix {
        0 => FirmwareSource::DownloadThenLocal,
        1 => FirmwareSource::Download,
        _ => FirmwareSource::Local,
    };

    let needs_net = !matches!(cfg.firmware.source, FirmwareSource::Local);
    let needs_local = !matches!(cfg.firmware.source, FirmwareSource::Download);

    if needs_net {
        println!("\nFUS needs the model + region you want to download for.");
        println!("(These are your download choices, not a hardcoded target.)");
        cfg.device.model = ask("  model to fetch (e.g. SM-A125F)", &cfg.device.model)?;
        cfg.device.region = ask("  region / CSC (e.g. XSG, EUX)", &cfg.device.region)?;
    }
    if needs_local {
        cfg.firmware.local_ap = ask(
            "\n  local firmware file (AP_*.tar.md5 / super.img.lz4 / super.img)",
            &cfg.firmware.local_ap,
        )?;
    }
    cfg.firmware.keep_download = ask_bool("\nKeep the downloaded firmware in the cache?", true)?;

    // ---- compatibility policy ----
    let compat_ix = ask_choice(
        "\nCompatibility policy (accepting firmware from similar phones)",
        &[
            (
                "soc+layout",
                "same SoC (from build.prop) + verified LP layout",
            ),
            ("model-prefix", "any model sharing the name prefix"),
            ("curated", "only exact / curated models"),
        ],
        0,
    )?;
    cfg.device.compat = match compat_ix {
        0 => CompatPolicy::SocLayout,
        1 => CompatPolicy::ModelPrefix,
        _ => CompatPolicy::Curated,
    };

    // ---- partition ----
    cfg.patch.partition = ask("\nPartition inside super to patch", &cfg.patch.partition)?;
    // ---- package ----
    cfg.package.tar_name = ask(
        "\nOutput tar name ({device} filled from build.prop)",
        &cfg.package.tar_name,
    )?;

    cfg.save(build_path)
        .with_context(|| format!("write {}", build_path.display()))?;
    println!("\nwrote {}", build_path.display());

    // ---- settings.toml (machine resources) ----
    if !settings_path.exists() {
        println!("\n== machine settings (cores / memory / toolchain) ==");
        let mut settings = Settings::default();
        let cpus = num_cpus::get();
        let jobs = ask(
            &format!("Parallel jobs (0 = auto-detect, this machine has {cpus})"),
            "0",
        )?;
        settings.build.jobs = jobs.parse().unwrap_or(0);
        let mem = ask(
            "Memory budget for heavy stages (MiB)",
            &settings.build.mem_budget_mb.to_string(),
        )?;
        settings.build.mem_budget_mb = mem.parse().unwrap_or(settings.build.mem_budget_mb);
        settings.toolchain.zig = ask(
            "Zig compiler path (blank = auto-discover bundled/PATH)",
            &settings.toolchain.zig,
        )?;
        settings
            .save(settings_path)
            .with_context(|| format!("write {}", settings_path.display()))?;
        println!("wrote {}", settings_path.display());
    } else {
        println!("keeping existing {}", settings_path.display());
    }

    let _ = root; // reserved for future validation against project layout
    println!("\nReady. Run `mmbuild build` to produce the flashable tar.");
    println!("Optional: `mmbuild doctor` to verify zig / firmware / profiles.");
    Ok(())
}

/// Remove work/cache dirs and zig stamps under rootfs/ for a clean rebuild.
pub fn clean(root: &Path, settings: &Settings) -> Result<()> {
    for d in [&settings.paths.work, &settings.paths.cache] {
        let p = if Path::new(d).is_absolute() {
            std::path::PathBuf::from(d)
        } else {
            root.join(d)
        };
        if p.exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("remove {}", p.display()))?;
            println!("removed {}", p.display());
        }
    }
    let rootfs = root.join("rootfs");
    if rootfs.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&rootfs) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("stamp") {
                    std::fs::remove_file(&p).ok();
                    println!("removed {}", p.display());
                }
            }
        }
    }
    Ok(())
}

/// Toolchain/config readiness; Ok(true) if hard checks pass.
pub fn doctor(root: &Path, build_path: &Path, settings_path: &Path) -> Result<bool> {
    use crate::config::FirmwareSource;
    use crate::{compat, toolchain};

    let mut ok = true;
    let mut check = |label: &str, pass: bool, detail: &str| {
        let mark = if pass { "ok  " } else { "FAIL" };
        if !pass {
            ok = false;
        }
        println!("{mark}  {label}: {detail}");
    };

    println!("mmbuild doctor — root {}\n", root.display());

    // build.toml
    if build_path.exists() {
        match BuildConfig::load(build_path) {
            Ok(cfg) => {
                check(
                    "build.toml",
                    true,
                    &format!(
                        "{} (model={:?}, source={:?})",
                        build_path.display(),
                        cfg.device.model,
                        cfg.firmware.source
                    ),
                );

                // Firmware path / FUS fields
                match cfg.firmware.source {
                    FirmwareSource::Local => {
                        if cfg.firmware.local_ap.trim().is_empty() {
                            check(
                                "firmware",
                                false,
                                "[firmware].local_ap is empty (required for source = local)",
                            );
                        } else {
                            let p = resolve_path(root, &cfg.firmware.local_ap);
                            check(
                                "firmware",
                                p.exists(),
                                &if p.exists() {
                                    format!("local_ap {}", p.display())
                                } else {
                                    format!("local_ap missing: {}", p.display())
                                },
                            );
                        }
                    }
                    FirmwareSource::Download => {
                        let ready = !cfg.device.model.trim().is_empty()
                            && !cfg.device.region.trim().is_empty();
                        check(
                            "firmware",
                            ready,
                            &if ready {
                                format!("FUS download {}/{}", cfg.device.model, cfg.device.region)
                            } else {
                                "[device].model and [device].region required for source = download"
                                    .into()
                            },
                        );
                    }
                    FirmwareSource::DownloadThenLocal => {
                        let fus_ok = !cfg.device.model.trim().is_empty()
                            && !cfg.device.region.trim().is_empty();
                        let local = if cfg.firmware.local_ap.trim().is_empty() {
                            None
                        } else {
                            Some(resolve_path(root, &cfg.firmware.local_ap))
                        };
                        let local_ok = local.as_ref().map(|p| p.exists()).unwrap_or(false);
                        let pass = fus_ok || local_ok;
                        let detail = match (fus_ok, local.as_ref()) {
                            (true, Some(p)) if p.exists() => format!(
                                "FUS {}/{} + local fallback {}",
                                cfg.device.model,
                                cfg.device.region,
                                p.display()
                            ),
                            (true, _) => format!(
                                "FUS {}/{} (local fallback unset or missing)",
                                cfg.device.model, cfg.device.region
                            ),
                            (false, Some(p)) if p.exists() => {
                                format!("local fallback only: {}", p.display())
                            }
                            _ => "need FUS model+region and/or an existing local_ap".into(),
                        };
                        check("firmware", pass, &detail);
                    }
                }

                // Device profile
                if cfg.device.model.trim().is_empty() {
                    check(
                        "profile",
                        true,
                        "no [device].model pinned (compile target stays as in build.toml)",
                    );
                } else {
                    match compat::parse_profiles(root) {
                        Ok(profiles) => {
                            let matched =
                                compat::match_profile(&profiles, &cfg.device.model, "", "");
                            match matched {
                                Some(p) => check(
                                    "profile",
                                    true,
                                    &format!(
                                        "matched {} ({}) target={} mcpu={}",
                                        p.model, p.rel_path, p.arch_triple, p.mcpu
                                    ),
                                ),
                                None => check(
                                    "profile",
                                    false,
                                    &format!(
                                        "no device_compat header for {:?} ({} profile(s) under src/device_compat/devices)",
                                        cfg.device.model,
                                        profiles.len()
                                    ),
                                ),
                            }
                        }
                        Err(e) => check("profile", false, &format!("parse failed: {e:#}")),
                    }
                }
            }
            Err(e) => check("build.toml", false, &format!("parse failed: {e:#}")),
        }
    } else {
        check(
            "build.toml",
            false,
            &format!("missing {} — run `mmbuild init`", build_path.display()),
        );
    }

    // settings.toml (optional; defaults are fine)
    let settings = Settings::load_or_default(settings_path)?;
    if settings_path.exists() {
        check(
            "settings.toml",
            true,
            &format!(
                "{} (jobs={}, cache={}, work={})",
                settings_path.display(),
                settings.effective_jobs(),
                settings.paths.cache,
                settings.paths.work
            ),
        );
    } else {
        check(
            "settings.toml",
            true,
            &format!(
                "absent — using defaults (jobs={}, cache={}, work={})",
                settings.effective_jobs(),
                settings.paths.cache,
                settings.paths.work
            ),
        );
    }

    // Zig
    match toolchain::find_zig(root, &settings) {
        Ok(z) => check("zig", true, &z.display().to_string()),
        Err(e) => check("zig", false, &format!("{e:#}")),
    }

    // Cache / work dirs (create-ability, not required to exist yet)
    for (label, rel) in [
        ("cache dir", settings.paths.cache.as_str()),
        ("work dir", settings.paths.work.as_str()),
    ] {
        let p = resolve_path(root, rel);
        if p.exists() {
            check(label, p.is_dir(), &format!("{} (exists)", p.display()));
        } else {
            // Parent must be writable for the build to create it.
            let parent = p.parent().unwrap_or(root);
            check(
                label,
                parent.exists() || parent == root,
                &format!("{} (will be created on build)", p.display()),
            );
        }
    }

    println!(
        "\ndoctor: {}",
        if ok {
            "ready"
        } else {
            "problems found — fix FAIL lines above before `mmbuild build`"
        }
    );
    Ok(ok)
}

fn resolve_path(root: &Path, p: &str) -> std::path::PathBuf {
    let pb = std::path::PathBuf::from(p);
    if pb.is_absolute() {
        pb
    } else {
        root.join(pb)
    }
}
