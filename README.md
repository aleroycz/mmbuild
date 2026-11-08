# mmbuild — MicroMiner OS builder

One cross-platform (Linux + Windows) Rust tool that does the whole flashable-image
pipeline, with Gradle-style logging. No external `img2simg` / `lz4` / `tar` and no
WSL loop-mount — a custom image library (`superimg`) handles everything.

**Nothing device-specific is hardcoded.** The model, codename and SoC are read from
the firmware's own `build.prop` at build time; the output tar name is derived from
that (`{device}` in `build.toml`).

## Crates

| crate      | role |
|------------|------|
| `mmcore`   | Gradle-style console logger (live progress bar, `> Task :x`, `BUILD SUCCESSFUL in …`) + humanizers |
| `superimg` | custom Android image tool — sparse (img2simg/simg2img), lz4 frame (`--content-size -B6`), dynamic-partitions/LP reader, **ext4 read+write** (replace file / sync tree, preserves SELinux xattrs, keeps gdt_csum crc16 valid), ustar tar. Usable as a library and as a standalone `superimg` CLI. |
| `mmbuild`  | the orchestrator + CLI |

## Build the tool

```sh
cargo build --release
# binaries: target/release/mmbuild(.exe), target/release/superimg(.exe)
```

## Use it

```sh
# 1. Generate build.toml (+ settings.toml) by answering prompts:
mmbuild init

# 2. Produce the flashable tar:
mmbuild --root /path/to/project build      # -i for detail, -d for debug, -q for quiet

# Remove scratch dirs:
mmbuild clean
```

### Pipeline stages (each a Gradle task)

1. **acquireFirmware** — Samsung FUS download (samloader-style nonce/auth/ECB
   decrypt) and/or a local AP file; recursively unpacked (zip → `AP_*.tar` →
   `super.img.lz4` → sparse → raw) by magic detection.
2. **readDeviceProps** — opens the system partition's ext4 and parses `build.prop`.
3. **verifyCompat** — SoC + LP-layout verification (policy set in `build.toml`).
4. **compileDevice** — cross-compiles the device binary with bundled `zig cc`
   (`aarch64-linux-musl`, `-mcpu=cortex_a53`).
5. **buildWeb** — builds the web portal (`npx next build` by default).
6. **patchSystem** — in-place replaces files (e.g. `/system/bin/init`) and syncs
   the web tree into the partition, all through the `superimg` ext4 writer.
7. **repackSuper** — raw → sparse → lz4 → ustar tar.

## Config

- **`settings.toml`** (machine): `jobs` (0 = auto), `mem_budget_mb`, `zig` path,
  `web_builder` command, `cache`/`work` dirs.
- **`build.toml`** (project): firmware `source`/`local_ap`, `device.compat`,
  compile `include`/`exclude`/`include_dirs`/`target`/`mcpu`/`cflags`/`output`,
  `web`, `patch.replace` + `partition`, `package.tar_name` (`{device}` templated).

> `exclude` drops files from the matched source set — e.g. `src/init/trampoline.c`,
> which defines its own `main()` and would otherwise clash with `main.c`.
