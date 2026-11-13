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

//! mmbuild CLI: firmware -> unpack -> patch system -> optional boot/recovery -> Odin tar.

mod android_img;
mod boot;
mod buildprop;
mod cli;
mod compat;
mod config;
mod fingerprint;
mod firmware;
mod format;
mod lint;
mod pipeline;
mod recovery;
mod subproject;
mod testing;
mod toolchain;
mod version;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mmcore::log::{Level, Logger};

use config::{BuildConfig, Settings};

#[derive(Parser)]
#[command(
    name = "mmbuild",
    about = "MicroMiner OS builder — fetch, compile, patch, repack, tar",
    version
)]
struct Cli {
    /// Project root (defaults to the current directory).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// build.toml path (relative to root unless absolute).
    #[arg(long, global = true, default_value = "build.toml")]
    config: PathBuf,

    /// settings.toml path (relative to root unless absolute).
    #[arg(long, global = true, default_value = "settings.toml")]
    settings: PathBuf,

    /// Quiet output (errors + build result only).
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Info output (show per-file detail).
    #[arg(short, long, global = true)]
    info: bool,
    /// Debug output (everything, incl. compiler chatter).
    #[arg(short, long, global = true)]
    debug: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactively generate build.toml (and settings.toml) from your answers.
    Init {
        /// Overwrite an existing build.toml without asking.
        #[arg(long)]
        force: bool,
    },
    /// Run the full build pipeline and produce the flashable tar.
    Build {
        /// Image targets to build and co-pack into the flashable tar.
        #[arg(long = "target", short = 't', value_name = "IMG")]
        targets: Vec<String>,
    },
    /// Build sub-projects (any folder with a `[subproject]` build.toml) and install their outputs
    /// in place.
    Sub {
        /// Only build the sub-project with this name (default: all).
        #[arg(long)]
        name: Option<String>,
        /// Also build sub-projects marked `manual = true`.
        #[arg(long)]
        all: bool,
    },
    /// Build custom recovery.img + Odin tar.
    Recovery,
    /// Build custom boot.img (bios /init) + Odin tar.
    Boot,
    /// Format first-party C sources in place (or `--check` only).
    Fmt {
        /// Don't write: list files that need formatting and exit non-zero.
        #[arg(long)]
        check: bool,
    },
    /// Format/lint first-party C (whitespace, spacing, trailing comments, line length).
    Format {
        /// Rewrite the offending files in place instead of only reporting them.
        #[arg(long)]
        apply: bool,
    },
    /// Compile and run C test programs under tests/.
    Test {
        /// Only run tests whose file name contains this substring.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Strict C warning gate via zig cc.
    Lint {
        /// Also run the clang static analyzer (path-sensitive bug finder).
        #[arg(long)]
        analyze: bool,
        /// Lint only this source group (e.g. "init" or "gui").
        #[arg(long)]
        group: Option<String>,
    },
    /// Remove the work/cache scratch directories.
    Clean,
    /// Check toolchain, zig, cache/work dirs, build.toml, and device profile.
    Doctor,
    /// Probe a connected phone over ADB and emit its build.toml or profile stub.
    Compat {
        /// Regenerate devices.inc from the headers; no phone.
        #[arg(long)]
        regen: bool,
        /// Wait for a phone to appear on USB before probing (up to 10 min).
        #[arg(long)]
        wait: bool,
    },
    /// Build identity in src/version.h; no flags prints version + provenance.
    Version {
        /// Bump the monotonic build number.
        #[arg(long)]
        bump_build: bool,
        /// Regenerate src/version_gen.h provenance.
        #[arg(long)]
        stamp: bool,
        /// Exit non-zero if semver equals HEAD (pre-commit gate).
        #[arg(long)]
        assert_bumped: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let level = if cli.debug {
        Level::Debug
    } else if cli.info {
        Level::Info
    } else if cli.quiet {
        Level::Quiet
    } else {
        Level::Lifecycle
    };
    let root_raw = cli
        .root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    /* Resolve `--root ..` without leaving `..` components (zig can't open those),
     * but strip Windows `\\?\` verbatim prefixes that break relative #includes. */
    let root = simplify_root(root_raw);
    let config_path = resolve(&root, &cli.config);
    let settings_path = resolve(&root, &cli.settings);

    let code = match &cli.cmd {
        Cmd::Init { force } => match cli::init(&root, &config_path, &settings_path, *force) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
        Cmd::Clean => {
            let settings = Settings::load_or_default(&settings_path).unwrap_or_default();
            match cli::clean(&root, &settings) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("error: {e:#}");
                    1
                }
            }
        }
        Cmd::Doctor => match cli::doctor(&root, &config_path, &settings_path) {
            Ok(ok) => {
                if ok {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
        Cmd::Build { targets } => run_build(&root, &config_path, &settings_path, level, targets),
        Cmd::Sub { name, all } => run_sub(&root, &settings_path, level, name.as_deref(), *all),
        Cmd::Recovery => run_recovery(&root, &config_path, &settings_path, level),
        Cmd::Boot => run_boot(&root, &config_path, &settings_path, level),
        Cmd::Fmt { check } => run_fmt(&root, &config_path, *check),
        Cmd::Format { apply } => run_format(&root, &config_path, *apply),
        Cmd::Test { filter } => run_test(&root, &config_path, &settings_path, filter.as_deref()),
        Cmd::Lint { analyze, group } => run_lint(
            &root,
            &config_path,
            &settings_path,
            *analyze,
            group.as_deref(),
        ),
        Cmd::Version {
            bump_build,
            stamp,
            assert_bumped,
        } => run_version(&root, *bump_build, *stamp, *assert_bumped),
        Cmd::Compat { regen, wait } => match compat::run(&root, *regen, *wait) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
    };
    std::process::exit(code);
}

/// `mmbuild fmt [--check]` - format first-party C sources, cargo-fmt style.
fn run_fmt(root: &Path, config_path: &Path, check: bool) -> i32 {
    use config::FormatSpec;
    let spec = match config_path.exists() {
        true => match BuildConfig::load(config_path) {
            Ok(cfg) => cfg.format,
            Err(e) => {
                eprintln!("error: {e:#}");
                return 1;
            }
        },
        false => FormatSpec::default(),
    };

    let rep = match format::run(root, &spec, check) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 1;
        }
    };

    // Lint notes are advisory; print a capped sample so output stays readable.
    if !rep.lint.is_empty() {
        let shown = rep.lint.len().min(20);
        for (path, line, msg) in rep.lint.iter().take(shown) {
            println!("lint {}:{} — {}", rel(root, path), line, msg);
        }
        if rep.lint.len() > shown {
            println!("lint … and {} more", rep.lint.len() - shown);
        }
    }
    for path in &rep.skipped {
        println!("skip {} (not UTF-8 text)", rel(root, path));
    }

    if check {
        if rep.changed.is_empty() {
            println!("fmt: {} file(s) already formatted", rep.scanned);
            0
        } else {
            println!("fmt: {} file(s) need formatting:", rep.changed.len());
            for path in &rep.changed {
                println!("  {}", rel(root, path));
            }
            println!("run `mmbuild fmt` to fix");
            1
        }
    } else if rep.changed.is_empty() {
        println!("fmt: {} file(s) already formatted", rep.scanned);
        0
    } else {
        println!(
            "fmt: reformatted {} of {} file(s):",
            rep.changed.len(),
            rep.scanned
        );
        for path in &rep.changed {
            println!("  {}", rel(root, path));
        }
        0
    }
}

/// .clang-format gate; --apply rewrites.
fn run_format(root: &Path, config_path: &Path, apply: bool) -> i32 {
    use config::FormatSpec;
    let spec = match config_path.exists() {
        true => match BuildConfig::load(config_path) {
            Ok(cfg) => cfg.format,
            Err(e) => {
                eprintln!("error: {e:#}");
                return 1;
            }
        },
        false => FormatSpec::default(),
    };

    // check = !apply: in report mode we never write.
    let rep = match format::run(root, &spec, !apply) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 1;
        }
    };

    // ColumnLimit (and any other) notes are advisory, not a gate.
    if !rep.lint.is_empty() {
        let shown = rep.lint.len().min(20);
        for (path, line, msg) in rep.lint.iter().take(shown) {
            println!("note {}:{} — {}", rel(root, path), line, msg);
        }
        if rep.lint.len() > shown {
            println!("note … and {} more", rep.lint.len() - shown);
        }
    }
    for path in &rep.skipped {
        println!("skip {} (not UTF-8 text)", rel(root, path));
    }

    if apply {
        if rep.changed.is_empty() {
            println!(
                "format: {} file(s) already match .clang-format",
                rep.scanned
            );
        } else {
            println!(
                "format: fixed {} of {} file(s):",
                rep.changed.len(),
                rep.scanned
            );
            for path in &rep.changed {
                println!("  {}", rel(root, path));
            }
        }
        0
    } else if rep.changed.is_empty() {
        println!("format: {} file(s) match .clang-format", rep.scanned);
        0
    } else {
        println!(
            "format: {} file(s) do not match .clang-format:",
            rep.changed.len()
        );
        for path in &rep.changed {
            println!("  {}", rel(root, path));
        }
        println!("run `mmbuild format --apply` to fix");
        1
    }
}

/// `mmbuild test [--filter]` - compile and run C tests.
fn run_test(root: &Path, config_path: &Path, settings_path: &Path, filter: Option<&str>) -> i32 {
    let cfg = match config_path.exists() {
        true => match BuildConfig::load(config_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e:#}");
                return 1;
            }
        },
        false => BuildConfig::template(),
    };
    let settings = Settings::load_or_default(settings_path).unwrap_or_default();

    let outcomes = match testing::run(root, &cfg.test, &settings, filter) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 1;
        }
    };

    if outcomes.is_empty() {
        println!("test: no test programs matched {:?}", cfg.test.include);
        return 0;
    }

    println!("running {} test(s)\n", outcomes.len());
    for o in &outcomes {
        let status = if o.passed {
            "ok  "
        } else if !o.compiled {
            "BUILD"
        } else if o.timed_out {
            "TIME"
        } else {
            "FAIL"
        };
        let detail = if o.passed {
            String::new()
        } else if !o.compiled {
            " (compile error)".into()
        } else if o.timed_out {
            " (timed out)".into()
        } else {
            match o.code {
                Some(c) => format!(" (exit {c})"),
                None => " (killed)".into(),
            }
        };
        println!("test {} ... {}{} [{} ms]", o.name, status, detail, o.ms);
        if !o.passed && !o.output.is_empty() {
            for line in o.output.lines() {
                println!("    {line}");
            }
        }
    }

    let passed = outcomes.iter().filter(|o| o.passed).count();
    let failed = outcomes.len() - passed;
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        passed,
        failed
    );
    if testing::all_passed(&outcomes) {
        0
    } else {
        1
    }
}

/// `mmbuild lint [--analyze] [--group <name>]` - strict-warnings gate.
fn run_lint(
    root: &Path,
    config_path: &Path,
    settings_path: &Path,
    analyze: bool,
    group: Option<&str>,
) -> i32 {
    use config::LintSpec;
    let spec = match config_path.exists() {
        true => match BuildConfig::load(config_path) {
            Ok(cfg) => cfg.lint,
            Err(e) => {
                eprintln!("error: {e:#}");
                return 1;
            }
        },
        false => LintSpec::default(),
    };
    let settings = Settings::load_or_default(settings_path).unwrap_or_default();
    let analyze_override = if analyze { Some(true) } else { None };

    let report = match lint::run(root, &spec, &settings, analyze_override, group) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 1;
        }
    };

    // Per-finding detail, most important first (errors before warnings).
    let mut idxs: Vec<usize> = (0..report.findings.len()).collect();
    idxs.sort_by_key(|&i| {
        if report.findings[i].kind == "error" {
            0
        } else {
            1
        }
    });
    for &i in &idxs {
        let f = &report.findings[i];
        let loc = if f.line == 0 {
            f.group.clone()
        } else {
            format!("{}:{}:{}", rel(root, &f.file), f.line, f.col)
        };
        let flag = if f.flag.is_empty() {
            String::new()
        } else {
            format!(" [{}]", f.flag)
        };
        println!("{} {} — {}{}", f.kind, loc, f.msg, flag);
    }

    let groups: Vec<String> = report
        .groups
        .iter()
        .map(|(n, c)| format!("{n}:{c}"))
        .collect();
    println!(
        "\nlint: {} file(s) [{}]{}, {} skipped (vendored)",
        report.scanned,
        groups.join(", "),
        if report.analyzed { " +analyzer" } else { "" },
        report.skipped,
    );
    if report.analyzed && report.analyze_unavailable > 0 {
        println!(
            "lint: note: {} file(s) only partially analyzed (zig cc --analyze \
             bails on cross-target system headers after the primary TU); \
             the strict warning gate still covers them fully",
            report.analyze_unavailable,
        );
    }

    match lint::gate(&report) {
        Ok(()) => {
            println!("lint: clean — no warnings");
            0
        }
        Err(e) => {
            println!("lint: {e}");
            1
        }
    }
}

/// `mmbuild version [--bump-build] [--stamp] [--assert-bumped]`.
fn run_version(root: &Path, bump_build: bool, stamp: bool, assert_bumped: bool) -> i32 {
    let res = (|| -> Result<()> {
        if assert_bumped {
            version::assert_bumped(root)?;
        }
        if bump_build {
            let info = version::bump_build(root)?;
            println!("version: build number -> {}", info.build);
        }
        if stamp {
            let info = version::read(root)?;
            let gi = version::git_info(root);
            version::stamp(root, &info, &gi)?;
            println!(
                "version: stamped src/version_gen.h ({})",
                info.full(&gi.short, gi.dirty)
            );
        }
        if !assert_bumped && !bump_build && !stamp {
            let info = version::read(root)?;
            let gi = version::git_info(root);
            println!("version    {}", info.semver());
            println!("build      {}", info.build);
            println!("full       {}", info.full(&gi.short, gi.dirty));
            println!(
                "commit     {} ({})",
                gi.short,
                if gi.dirty { "dirty" } else { "clean" }
            );
            println!("commit-long {}", gi.long);
            println!("built-by   {}", gi.built_by);
            if !gi.has_git {
                println!("note       not a git repo (provenance is local/unknown)");
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    }
}

/// Path relative to root for display (falls back to the full path).
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn run_build(
    root: &Path,
    config_path: &Path,
    settings_path: &Path,
    level: Level,
    targets: &[String],
) -> i32 {
    let log = Logger::init(level);
    log.set_phase("CONFIGURING");
    let result = (|| -> Result<pipeline::Built> {
        if !config_path.exists() {
            anyhow::bail!("no {} — run `mmbuild init` first", config_path.display());
        }
        let bt = pipeline::BuildTargets::parse(targets)?;
        let mut cfg = BuildConfig::load(config_path)
            .with_context(|| format!("load {}", config_path.display()))?;
        let settings = Settings::load_or_default(settings_path)?;
        // sync registry + pin compat target for the on-device BIOS check
        compat::regen_inc(root)?;
        compat::write_target(root, &cfg.device.model)?;
        // the device_compat profile is authoritative for the compile target: inherit
        // arch_triple/mcpu when unset, refuse to build on drift
        for note in compat::reconcile_target(
            root,
            &cfg.device.model,
            &mut cfg.source.target,
            &mut cfg.source.mcpu,
        )? {
            if note.starts_with("no device_compat profile") {
                log.warn(&note);
            } else {
                log.info(&note);
            }
        }
        // bump build number + stamp provenance before compiling
        let (vinfo, ginfo) = version::prepare(root)?;
        log.lifecycle(&format!(
            "version {} (build {}, {})",
            vinfo.semver(),
            vinfo.build,
            ginfo.short
        ));
        pipeline::run(root, &cfg, &settings, bt, &log)
    })();

    match result {
        Ok(built) => {
            log.lifecycle(&format!("done: {} ({})", built.tar.display(), built.device));
            log.finish(true);
            0
        }
        Err(e) => {
            log.error(&format!("{e:#}"));
            log.finish(false);
            1
        }
    }
}

/// `mmbuild sub [--name <n>] [--all]` - build sub-projects, place files, no tar.
fn run_sub(root: &Path, settings_path: &Path, level: Level, name: Option<&str>, all: bool) -> i32 {
    let log = Logger::init(level);
    log.set_phase("SUBPROJECTS");
    let settings = Settings::load_or_default(settings_path).unwrap_or_default();
    // best-effort pinned model, so a standalone `sub` build reconciles its compile target against
    // the device profile like a full build
    let model = BuildConfig::load(&root.join("build.toml"))
        .map(|c| c.device.model)
        .unwrap_or_default();
    match subproject::build_all(root, &settings, &log, name, all || name.is_some(), &model) {
        Ok(built) => {
            if built.is_empty() {
                log.lifecycle("no sub-projects built");
            } else {
                log.lifecycle(&format!(
                    "built {} sub-project(s): {}",
                    built.len(),
                    built.join(", ")
                ));
            }
            log.finish(true);
            0
        }
        Err(e) => {
            log.error(&format!("{e:#}"));
            log.finish(false);
            1
        }
    }
}

/// `mmbuild recovery` - build the custom recovery image + flashing artifacts.
fn run_recovery(root: &Path, config_path: &Path, settings_path: &Path, level: Level) -> i32 {
    let log = Logger::init(level);
    log.set_phase("RECOVERY");
    let result = (|| -> Result<recovery::RecoveryArtifacts> {
        if !config_path.exists() {
            anyhow::bail!("no {} — run `mmbuild init` first", config_path.display());
        }
        let cfg = BuildConfig::load(config_path)
            .with_context(|| format!("load {}", config_path.display()))?;
        let settings = Settings::load_or_default(settings_path)?;
        let device = if cfg.device.model.is_empty() {
            "recovery".to_string()
        } else {
            cfg.device.model.clone()
        };
        recovery::build(root, &cfg, &settings, &device, &log)
    })();
    match result {
        Ok(a) => {
            log.lifecycle(&format!("done: {}", a.img.display()));
            log.finish(true);
            0
        }
        Err(e) => {
            log.error(&format!("{e:#}"));
            log.finish(false);
            1
        }
    }
}

/// `mmbuild boot` - build the custom boot image (Micro BIOS ramdisk) + tar.
fn run_boot(root: &Path, config_path: &Path, settings_path: &Path, level: Level) -> i32 {
    let log = Logger::init(level);
    log.set_phase("BOOT");
    let result = (|| -> Result<boot::BootArtifacts> {
        if !config_path.exists() {
            anyhow::bail!("no {} — run `mmbuild init` first", config_path.display());
        }
        let cfg = BuildConfig::load(config_path)
            .with_context(|| format!("load {}", config_path.display()))?;
        let settings = Settings::load_or_default(settings_path)?;
        let device = if cfg.device.model.is_empty() {
            "boot".to_string()
        } else {
            cfg.device.model.clone()
        };
        boot::build(root, &cfg, &settings, &device, &log)
    })();
    match result {
        Ok(a) => {
            log.lifecycle(&format!("done: {}", a.img.display()));
            log.finish(true);
            0
        }
        Err(e) => {
            log.error(&format!("{e:#}"));
            log.finish(false);
            1
        }
    }
}

fn simplify_root(root_raw: PathBuf) -> PathBuf {
    let abs = if root_raw.is_absolute() {
        root_raw
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(root_raw)
    };
    let canon = std::fs::canonicalize(&abs).unwrap_or(abs);
    strip_verbatim_prefix(canon)
}

fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        /* `\\?\UNC\server\share\...` -> `\\server\share\...` */
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        return PathBuf::from(rest);
    }
    p
}

fn resolve(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}
