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

//! `settings.toml` + `build.toml`; `{device}` filled from build.prop at build time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ----------------------------- settings.toml -----------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub build: BuildSettings,
    pub toolchain: Toolchain,
    pub paths: Paths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildSettings {
    /// Parallel compile jobs. 0 => detect CPU count.
    pub jobs: usize,
    /// Soft memory budget (MiB) for heavy stages (sparse/lz4 buffers, logging).
    pub mem_budget_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Toolchain {
    /// Path to the Zig compiler (used as `zig cc`). Empty => auto-discover.
    pub zig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Paths {
    /// Firmware download cache.
    pub cache: String,
    /// Scratch working dir for raw images / intermediates.
    pub work: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            build: BuildSettings {
                jobs: 0,
                mem_budget_mb: 4096,
            },
            toolchain: Toolchain { zig: String::new() },
            paths: Paths {
                cache: ".mmcache".into(),
                work: ".mmwork".into(),
            },
        }
    }
}

impl Default for BuildSettings {
    fn default() -> Self {
        Settings::default().build
    }
}
impl Default for Toolchain {
    fn default() -> Self {
        Settings::default().toolchain
    }
}
impl Default for Paths {
    fn default() -> Self {
        Settings::default().paths
    }
}

impl Settings {
    pub fn effective_jobs(&self) -> usize {
        if self.build.jobs == 0 {
            num_cpus::get()
        } else {
            self.build.jobs
        }
    }

    pub fn load_or_default(path: &Path) -> Result<Settings> {
        if path.exists() {
            let s = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            Ok(toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?)
        } else {
            Ok(Settings::default())
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

// ------------------------------ build.toml -------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    pub device: Device,
    pub firmware: Firmware,
    pub source: SourceSpec,
    pub patch: PatchSpec,
    pub package: PackageSpec,
    /// Formatter/linter settings (`mmbuild fmt`). Optional in build.toml.
    #[serde(default)]
    pub format: FormatSpec,
    /// Test-runner settings (`mmbuild test`). Optional in build.toml.
    #[serde(default)]
    pub test: TestSpec,
    /// Strict warnings/static-analysis linter (`mmbuild lint`). Optional.
    #[serde(default)]
    pub lint: LintSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    /// Model the user chose to fetch firmware for, e.g. "SM-A125F" (Samsung) or a codename like
    /// "oriole" (Pixel).
    pub model: String,
    /// CSC/region, e.g. "XSG", "EUX" (Samsung only).
    pub region: String,
    /// Firmware vendor: samsung | pixel | url backend.
    #[serde(default)]
    pub brand: String,
    /// Compatibility policy for accepting firmware from similar phones.
    pub compat: CompatPolicy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CompatPolicy {
    /// Group by SoC (read from build.prop) then verify super/LP geometry.
    SocLayout,
    /// Accept any model sharing a prefix.
    ModelPrefix,
    /// Only models listed in compat.toml.
    Curated,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FirmwareSource {
    /// Try FUS download, fall back to the local file.
    DownloadThenLocal,
    /// Online FUS only.
    Download,
    /// Local file only.
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Firmware {
    pub source: FirmwareSource,
    /// Local AP tar / super.img.lz4 / super.img, used for Local / fallback.
    pub local_ap: String,
    /// Direct download URL for a factory/firmware archive (any brand).
    #[serde(default)]
    pub url: String,
    /// Keep the downloaded+decrypted firmware in the cache.
    pub keep_download: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpec {
    /// Source globs (relative to project root) for the device binary.
    pub include: Vec<String>,
    /// Sources to drop from the matched set (e.g. an alternate `main`).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Path substrings marking a matched source as vendored/third-party.
    #[serde(default)]
    pub third_party: Vec<String>,
    pub include_dirs: Vec<String>,
    pub target: String,
    pub mcpu: String,
    pub cflags: Vec<String>,
    /// Output binary path (the patched /system/bin/init).
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchSpec {
    /// In-place file replacements inside the system partition.
    pub replace: Vec<Replacement>,
    /// Partition inside super to patch (usually "system").
    pub partition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replacement {
    /// Absolute path inside the partition, e.g. /system/bin/init.
    pub path: String,
    /// Local source file (relative to project root).
    pub from: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageSpec {
    /// Output tar name; `{device}` is filled from build.prop at build time.
    pub tar_name: String,
    /// Members placed in the Odin tar.
    pub members: Vec<String>,
}

/// `mmbuild fmt` - a whitespace-level formatter + linter for the first-party C sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatSpec {
    /// Globs of sources/headers to format (relative to root).
    pub include: Vec<String>,
    /// Vendored path substrings; format never touches them.
    pub exclude: Vec<String>,
    /// Indent width in spaces; a leading tab advances to the next multiple.
    pub indent_width: usize,
    /// Expand tabs everywhere, not just in the indent. Off by default so a tab inside a string/char
    /// literal is left alone.
    pub expand_all_tabs: bool,
    /// Collapse runs of blank lines down to at most this many.
    pub max_blank_lines: usize,
    /// Soft line-length the linter reports on (0 disables the check).
    pub max_line_len: usize,
}

impl Default for FormatSpec {
    fn default() -> Self {
        FormatSpec {
            include: vec!["src/**/*.c".into(), "src/**/*.h".into()],
            exclude: vec!["third_party".into()],
            indent_width: 4,
            expand_all_tabs: false,
            max_blank_lines: 1,
            max_line_len: 100,
        }
    }
}

/// `mmbuild test` - host zig cc compile+run of standalone programs under tests/.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TestSpec {
    /// Test source globs (relative to root).
    pub include: Vec<String>,
    /// `-I` directories for test compiles.
    pub include_dirs: Vec<String>,
    /// `zig cc` target triple; empty => native host.
    pub target: String,
    pub cflags: Vec<String>,
    /// Directory for built test executables.
    pub out_dir: String,
    /// Per-test run timeout (seconds); 0 disables the limit.
    pub timeout_secs: u64,
}

impl Default for TestSpec {
    fn default() -> Self {
        TestSpec {
            include: vec!["tests/**/*.c".into()],
            include_dirs: vec!["tests/shim".into(), "src".into(), "src/third_party".into()],
            target: String::new(),
            cflags: vec![
                "-O2".into(),
                "-std=c11".into(),
                "-D_GNU_SOURCE".into(),
                "-Wall".into(),
                "-Wextra".into(),
            ],
            out_dir: "build/tests".into(),
            timeout_secs: 30,
        }
    }
}

/// `mmbuild lint` - the strict-warnings gate ("clippy for our C").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LintSpec {
    /// Shared warning flags for every lint group.
    pub warnings: Vec<String>,
    /// Vendored path substrings; never linted.
    pub exclude: Vec<String>,
    /// Enable clang --analyze (mmbuild lint --analyze).
    pub analyze: bool,
    /// The source groups to lint, each with its own std/defines/includes.
    pub groups: Vec<LintGroup>,
}

/// One lintable source group (mirrors a compile unit's flags).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintGroup {
    /// Short label shown in output, e.g. "init" or "gui".
    pub name: String,
    /// Source globs (relative to root).
    pub include: Vec<String>,
    /// Include dirs passed to the compiler.
    pub include_dirs: Vec<String>,
    /// `zig cc` target triple; empty => native.
    pub target: String,
    /// `-mcpu=` value; empty => default.
    pub mcpu: String,
    /// Group cflags; shared warnings appended.
    pub cflags: Vec<String>,
}

impl Default for LintSpec {
    fn default() -> Self {
        LintSpec {
            warnings: vec![
                "-Wall".into(),
                "-Wextra".into(),
                "-Wconversion".into(),
                "-Wsign-conversion".into(),
                "-Wshadow".into(),
                "-Wstrict-prototypes".into(),
                "-Wmissing-prototypes".into(),
                "-Wpointer-arith".into(),
                "-Wcast-qual".into(),
            ],
            exclude: vec![
                "trampoline".into(),
                "qrcodegen".into(),
                "font8x16".into(),
                "stb_truetype".into(),
                "tweetnacl".into(),
            ],
            analyze: false,
            groups: vec![
                LintGroup {
                    name: "init".into(),
                    include: vec![
                        "src/common/**/*.c".into(),
                        "src/init/**/*.c".into(),
                        "src/platform/**/*.c".into(),
                        "src/storage/**/*.c".into(),
                        "src/security/**/*.c".into(),
                        "src/ipc/**/*.c".into(),
                        "src/log/**/*.c".into(),
                        "src/net/**/*.c".into(),
                        "src/miner/**/*.c".into(),
                        "src/ota/**/*.c".into(),
                        "src/device_compat/**/*.c".into(),
                    ],
                    include_dirs: vec!["src".into(), "src/third_party".into()],
                    target: "aarch64-linux-musl".into(),
                    mcpu: "cortex_a53".into(),
                    /* Match device compile feature macros so POSIX/GNU APIs are declared. */
                    cflags: vec!["-std=c11".into(), "-D_GNU_SOURCE".into()],
                },
                LintGroup {
                    name: "gui".into(),
                    include: vec!["src/gui/**/*.c".into()],
                    include_dirs: vec!["src".into(), "src/gui".into(), "src/third_party".into()],
                    target: "aarch64-linux-musl".into(),
                    mcpu: "cortex_a53".into(),
                    cflags: vec![
                        "-std=c11".into(),
                        "-D_GNU_SOURCE".into(),
                        "-DMMGUI_ENABLE_TTF".into(),
                    ],
                },
            ],
        }
    }
}

impl BuildConfig {
    pub fn load(path: &Path) -> Result<BuildConfig> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&s).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    /// A reasonable default derived from the current project layout. Used as the starting point the
    /// wizard then edits.
    pub fn template() -> BuildConfig {
        BuildConfig {
            device: Device {
                model: String::new(),
                region: String::new(),
                brand: String::new(),
                compat: CompatPolicy::SocLayout,
            },
            firmware: Firmware {
                source: FirmwareSource::DownloadThenLocal,
                local_ap: String::new(),
                url: String::new(),
                keep_download: true,
            },
            source: SourceSpec {
                include: vec![
                    "src/asm/**/*.S".into(),
                    "src/common/**/*.c".into(),
                    "src/init/**/*.c".into(),
                    "src/ipc/**/*.c".into(),
                    "src/log/**/*.c".into(),
                    "src/platform/**/*.c".into(),
                    "src/storage/**/*.c".into(),
                    "src/security/**/*.c".into(),
                    "src/net/**/*.c".into(),
                    "src/miner/**/*.c".into(),
                    "src/ota/**/*.c".into(),
                    "src/device_compat/**/*.c".into(),
                    "src/third_party/**/*.c".into(),
                ],
                exclude: vec!["src/init/trampoline.c".into()],
                third_party: vec!["qrcodegen".into(), "tweetnacl".into(), "verushash".into()],
                include_dirs: vec!["src".into(), "src/third_party".into()],
                target: "aarch64-linux-musl".into(),
                mcpu: "cortex_a53".into(),
                cflags: vec![
                    "-static".into(),
                    "-O3".into(),
                    "-Wall".into(),
                    "-Wextra".into(),
                    "-Werror".into(),
                    "-pthread".into(),
                    "-lm".into(),
                ],
                output: "rootfs/init".into(),
            },
            patch: PatchSpec {
                replace: vec![Replacement {
                    path: "/system/bin/init".into(),
                    from: "rootfs/init".into(),
                }],
                partition: "system".into(),
            },
            package: PackageSpec {
                tar_name: "{device}_System_OS.tar".into(),
                members: vec!["super.img.lz4".into()],
            },
            format: FormatSpec::default(),
            test: TestSpec::default(),
            lint: LintSpec::default(),
        }
    }
}

// ---------------------------- sub-projects -------------------------------
// Nested `build.toml` with `[subproject]`; outputs install in-tree (no sub-project tar).

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubprojectConfig {
    pub subproject: SubprojectMeta,
    /// C binary compile spec (kind = "cbinary"); same fields as root `[source]`.
    #[serde(default)]
    pub build: Option<SourceSpec>,
    /// External dependency fetch/extract/build spec (kind = "external").
    #[serde(default)]
    pub external: Option<ExternalSpec>,
    /// Files to copy into place after building (the only "output" of a sub-project - no tar is ever
    /// produced here).
    #[serde(default)]
    pub install: Vec<InstallSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubprojectMeta {
    /// Unique short name, e.g. "mmgui", "recovery", "libnl".
    pub name: String,
    /// What kind of sub-project this is.
    pub kind: SubKind,
    /// Other sub-projects (by name) that must be built first.
    #[serde(default)]
    pub needs: Vec<String>,
    /// One-line human description (shown in logs).
    #[serde(default)]
    pub description: String,
    /// manual=true: skip unless named / --all.
    #[serde(default)]
    pub manual: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SubKind {
    /// Cross/host compile a C binary with `zig cc` from a `[build]` spec.
    Cbinary,
    /// Fetch + verify + extract (and optionally build) an `external/` dep.
    External,
    /// No build step; only run the `[[install]]` copies (stage static files).
    Copy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalSpec {
    /// External archive URL; skip if dir exists.
    #[serde(default)]
    pub url: String,
    /// Expected SHA-256 of the downloaded archive (hex). Verified before use.
    #[serde(default)]
    pub sha256: String,
    /// Local archive path (relative to root). Defaults to the URL's basename placed under
    /// `external/`.
    #[serde(default)]
    pub archive: String,
    /// Directory the archive extracts to / the dep lives in (relative to root).
    pub dir: String,
    /// Optional build command run inside `dir` (program + args). Skipped when every `artifacts`
    /// entry already exists.
    #[serde(default)]
    pub build_command: Vec<String>,
    /// Files (relative to `dir`) that the build must produce; their existence is how mmbuild
    /// decides the.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSpec {
    /// Source path (relative to root), e.g. "build/mmgui".
    pub from: String,
    /// Destination path (relative to root), e.g. "rootfs/mmgui".
    pub to: String,
    /// Octal mode for the installed file (e.g. "0755"). Optional.
    #[serde(default)]
    pub mode: String,
}

impl SubprojectConfig {
    pub fn load(path: &Path) -> Result<SubprojectConfig> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&s).with_context(|| format!("parse sub-project {}", path.display()))
    }
}

/// Discover subproject build.toml (needs-first order).
pub fn discover_subprojects(root: &Path) -> Result<Vec<(PathBuf, SubprojectConfig)>> {
    use walkdir::WalkDir;
    const SKIP: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        ".mmwork",
        ".mmcache",
        "build",
        "bin",
    ];
    let mut found: Vec<(PathBuf, SubprojectConfig)> = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            // Skip known-heavy dirs (but always allow the root itself).
            if e.depth() == 0 {
                return true;
            }
            !(e.file_type().is_dir()
                && e.file_name()
                    .to_str()
                    .map(|n| SKIP.contains(&n))
                    .unwrap_or(false))
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() || entry.file_name() != "build.toml" {
            continue;
        }
        let path = entry.path();
        // The root build.toml has no [subproject] table, so this parse fails and it is skipped. A
        // quick text probe avoids a noisy parse attempt.
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !text.contains("[subproject]") {
            continue;
        }
        let dir = path.parent().unwrap_or(root).to_path_buf();
        match SubprojectConfig::load(path) {
            Ok(cfg) => found.push((dir, cfg)),
            Err(e) => {
                // A malformed sub-project is a hard error - better to fail loudly than silently
                // skip something the user expects to be built.
                return Err(e);
            }
        }
    }
    Ok(order_by_needs(found))
}

/// Topo-sort by needs; cycles fall back to discovery order.
fn order_by_needs(items: Vec<(PathBuf, SubprojectConfig)>) -> Vec<(PathBuf, SubprojectConfig)> {
    let mut remaining = items;
    let mut done: Vec<String> = Vec::new();
    let mut out: Vec<(PathBuf, SubprojectConfig)> = Vec::new();
    while !remaining.is_empty() {
        // Pick the first whose needs are all satisfied.
        let idx = remaining
            .iter()
            .position(|(_, c)| c.subproject.needs.iter().all(|n| done.contains(n)));
        match idx {
            Some(i) => {
                let (p, c) = remaining.remove(i);
                done.push(c.subproject.name.clone());
                out.push((p, c));
            }
            None => {
                // Unsatisfiable (cycle or external need): take them in order.
                for (p, c) in remaining.drain(..) {
                    out.push((p, c));
                }
            }
        }
    }
    out
}

/// Resolve `{device}` in a template string using the build.prop device name.
pub fn expand_device(template: &str, device: &str) -> String {
    template.replace("{device}", device)
}
