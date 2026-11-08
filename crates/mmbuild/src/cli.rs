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

//! Interactive CLI: the `init` wizard writes `build.toml` from the user's
//! answers, and `settings.toml` captures machine resources (cores, memory,
//! toolchain paths). No device identity is baked in — the wizard only records
//! *choices* (which firmware to fetch, where the local AP file is); the real
//! device name is read from build.prop at build time.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{
    BuildConfig, CompatPolicy, FirmwareSource, Settings,
};

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
            ("download+local", "try Samsung FUS, fall back to a local file"),
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
    cfg.firmware.keep_download =
        ask_bool("\nKeep the downloaded firmware in the cache?", true)?;

    // ---- compatibility policy ----
    let compat_ix = ask_choice(
        "\nCompatibility policy (accepting firmware from similar phones)",
        &[
            ("soc+layout", "same SoC (from build.prop) + verified LP layout"),
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

    // ---- partition + web ----
    cfg.patch.partition = ask(
        "\nPartition inside super to patch",
        &cfg.patch.partition,
    )?;
    cfg.web.enabled = ask_bool("Build and embed the web portal?", cfg.web.enabled)?;
    if cfg.web.enabled {
        cfg.web.dir = ask("  web project dir", &cfg.web.dir)?;
        cfg.web.dest = ask("  destination inside partition", &cfg.web.dest)?;
    }

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
        settings.toolchain.web_builder = ask(
            "Web build command",
            &settings.toolchain.web_builder,
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
    Ok(())
}

/// `clean`: remove the work/cache scratch dirs named in settings.toml.
pub fn clean(root: &Path, settings: &Settings) -> Result<()> {
    for d in [&settings.paths.work, &settings.paths.cache] {
        let p = if Path::new(d).is_absolute() {
            std::path::PathBuf::from(d)
        } else {
            root.join(d)
        };
        if p.exists() {
            std::fs::remove_dir_all(&p)
                .with_context(|| format!("remove {}", p.display()))?;
            println!("removed {}", p.display());
        }
    }
    Ok(())
}
