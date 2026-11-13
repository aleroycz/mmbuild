# mmbuild

The build tool for MicroMiner OS. It runs the whole flashable-image pipeline on
Linux or Windows: fetch firmware, cross-compile, patch the system image and pack
an Odin tar. It needs no `img2simg`, `lz4`, `tar` or loop mounts; the
`superimg` crate handles the image formats itself.

Nothing device-specific is hardcoded. Model, codename and SoC come from the
firmware's `build.prop`, and the output name is templated from them
(`{device}` in `build.toml`).

## Crates

| Crate | Purpose |
|-------|---------|
| `mmbuild` | CLI and build pipeline |
| `superimg` | Android image library and CLI: sparse, lz4 frames, LP metadata, ext4 read/write (keeps SELinux xattrs and group checksums valid), boot images, cpio, ustar |
| `mmadb` | Minimal ADB client used by `mmbuild compat` |
| `mmcore` | Console logging and progress output |

## Building

```sh
cargo build --release
```

This produces `target/release/mmbuild` and `target/release/superimg`. Run
`mmbuild` from the project root (the directory with `build.toml` and `src/`) or
pass `--root <path>`.

## Usage

```sh
mmbuild init          # write build.toml and settings.toml interactively
mmbuild doctor        # check zig, firmware source and device profile
mmbuild build         # build {device}_System_OS.tar (super image only)
mmbuild build -t all  # also build custom recovery and boot images
mmbuild clean         # remove .mmwork and .mmcache
```

Use `-q`, `-i` or `-d` for quiet, detailed or debug output.

| Command | Description |
|---------|-------------|
| `sub [--name N] [--all]` | Build and install sub-projects without packaging |
| `recovery` | Build recovery.img and a recovery-only Odin tar |
| `boot` | Build a custom boot.img (Micro BIOS ramdisk) and tar |
| `format [--apply]` | Check C sources against `.clang-format`, or fix them |
| `fmt [--check]` | Whitespace formatter for C sources |
| `test [--filter S]` | Compile and run the host tests in `tests/` |
| `lint [--analyze] [--group G]` | Strict warning gate, optionally with the clang analyzer |
| `compat [--regen] [--wait]` | Probe a phone over ADB or regenerate `devices.inc` |
| `version` | Show, bump or stamp `src/version.h` |

## Pipeline

`mmbuild build` runs these tasks in order:

1. **acquireFirmware**: Samsung FUS, Pixel factory images, a direct URL or a
   local AP file, unpacked down to a raw `super` image.
2. **readDeviceProps**: parse `build.prop` from the system partition.
3. **verifyCompat**: check SoC and partition layout against the device profile.
4. **buildSubprojects**: build every `build.toml` with a `[subproject]` table
   (mmgui, external deps) and install the outputs into `rootfs/`.
5. **compileDevice**: cross-compile `/system/bin/init` with `zig cc`.
6. **patchSystem**: replace files in the system ext4 and stage the runtime
   linker from `com.android.runtime.apex`.
7. **buildRecovery / buildBoot**: only with `-t recovery`, `-t boot` or `-t all`.
   Unsigned images lose Samsung's `SignerVer02` footer and usually bootloop
   (see [`docs/boot/SIGNERVER02.md`](../docs/boot/SIGNERVER02.md)), so the
   supported path is the soft Micro BIOS inside init.
8. **repackSuper**: raw -> sparse -> lz4 -> tar.

The pristine `super.raw` is cached under `.mmcache/firmware/<fingerprint>/`.
Each `zig cc` output gets a `.stamp` file and is relinked only when its sources,
headers or flags change. This is not a full dependency graph, so run
`mmbuild clean` if something looks stale.

## Configuration

`settings.toml` is per machine and optional: `jobs` (0 = auto),
`mem_budget_mb`, the `zig` path and the `cache` / `work` directories.

`build.toml` describes the project:

| Section | Keys |
|---------|------|
| `[device]` | `brand`, `model`, `region`, `compat` (`soc-layout`, `model-prefix`, `curated`) |
| `[firmware]` | `source` (`download`, `download-then-local`, `local`), `local_ap`, `url`, `keep_download` |
| `[source]` | `include`, `exclude`, `third_party`, `include_dirs`, `target`, `mcpu`, `cflags`, `output` |
| `[patch]` | `partition`, plus `[[patch.replace]]` entries with `path` and `from` |
| `[package]` | `tar_name`, `members` |
| `[format]`, `[test]`, `[lint]` | optional overrides |

Sub-projects have their own `build.toml` with a `[subproject]` table
(`kind = "cbinary" | "external" | "copy"`) and `[build]`, `[external]` or
`[[install]]` sections as needed.
