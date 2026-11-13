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

//! Standalone superimg CLI for image inspect/patch outside mmbuild.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mmcore::human_bytes;
use superimg::{ext4::Ext4, lp, lz4frame, sparse};

#[derive(Parser)]
#[command(
    name = "superimg",
    about = "Android super/system image toolkit (pure Rust)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print LP partitions and the ext4 geometry/features of `system`.
    Info {
        /// Raw (de-sparsed) super image.
        image: PathBuf,
        /// Partition to inspect as ext4 (default: system).
        #[arg(long, default_value = "system")]
        partition: String,
    },
    /// List a directory inside a partition.
    Ls {
        image: PathBuf,
        path: String,
        #[arg(long, default_value = "system")]
        partition: String,
    },
    /// Extract a file from a partition to stdout-less output path.
    Extract {
        image: PathBuf,
        path: String,
        out: PathBuf,
        #[arg(long, default_value = "system")]
        partition: String,
    },
    /// Expand a sparse image to raw (`simg2img`).
    Unsparse { input: PathBuf, output: PathBuf },
    /// Encode a raw image to Android sparse (`img2simg`).
    Sparse {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, default_value_t = 4096)]
        block_size: u32,
    },
    /// Replace a regular file's contents in place (keeps inode + SELinux ctx).
    Replace {
        /// Raw super image (edited in place).
        image: PathBuf,
        /// Absolute path inside the partition, e.g. /system/bin/init.
        path: String,
        /// Local file whose bytes become the new contents.
        src: PathBuf,
        #[arg(long, default_value = "system")]
        partition: String,
    },
    /// Replace a directory subtree inside the partition with a local folder.
    SyncTree {
        image: PathBuf,
        /// Destination dir inside the partition, e.g. /www.
        dest: String,
        /// Local folder whose contents replace it.
        src: PathBuf,
        #[arg(long, default_value = "system")]
        partition: String,
    },
    /// Decompress a standard lz4 frame.
    Unlz4 { input: PathBuf, output: PathBuf },
    /// Compress to a standard lz4 frame (`lz4 --content-size -B6`).
    Lz4 { input: PathBuf, output: PathBuf },
}

fn resolve_partition(image: &Path, part: &str) -> Result<(u64, u64)> {
    let md = lp::read_metadata(image).context("read LP metadata")?;
    let p = md
        .partition_or_slot_a(part)
        .with_context(|| format!("partition '{part}' not found in super"))?;
    let off = p.first_offset().context("partition has no extent")?;
    Ok((off, p.size_bytes()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Info { image, partition } => {
            let md = lp::read_metadata(&image)?;
            println!(
                "LP metadata (logical block size {}):",
                md.logical_block_size
            );
            for p in &md.partitions {
                let off = p.first_offset().unwrap_or(0);
                println!(
                    "  {:<16} offset={:<12} size={:<14} ({}){}",
                    p.name,
                    off,
                    p.size_bytes(),
                    human_bytes(p.size_bytes()),
                    if p.is_contiguous() {
                        ""
                    } else {
                        "  [non-contiguous]"
                    }
                );
            }
            if let Some(p) = md.partition_or_slot_a(&partition) {
                let (off, size) = (p.first_offset().unwrap(), p.size_bytes());
                let mut fs = Ext4::open(&image, off, size, false)?;
                let sb = fs.superblock().clone();
                println!("\next4 '{partition}' @ offset {off}:");
                println!("  block size      : {}", sb.block_size);
                println!(
                    "  blocks          : {} (free {})",
                    sb.blocks_count, sb.free_blocks_count
                );
                println!(
                    "  inodes          : {} (free {})",
                    sb.inodes_count, sb.free_inodes_count
                );
                println!("  inode size      : {}", sb.inode_size);
                println!("  64-bit          : {}", fs.has_64bit());
                println!("  metadata_csum   : {}", fs.has_metadata_csum());
                println!("  feature_incompat: {:#010x}", sb.feature_incompat);
                println!("  feature_ro      : {:#010x}", sb.feature_ro_compat);
                println!(
                    "  free space      : {}",
                    human_bytes(sb.free_blocks_count * sb.block_size as u64)
                );
                if let Ok(ino) = fs
                    .lookup("/system/bin/init")
                    .or_else(|_| fs.lookup("/init"))
                {
                    if let Ok(Some(ctx)) = fs.selinux_context(ino) {
                        println!("  /system/bin/init selinux: {ctx}");
                    }
                }
            }
        }
        Cmd::Ls {
            image,
            path,
            partition,
        } => {
            let (off, size) = resolve_partition(&image, &partition)?;
            let mut fs = Ext4::open(&image, off, size, false)?;
            for e in fs.list_dir(&path)? {
                let kind = match e.file_type {
                    2 => "d",
                    1 => "-",
                    _ => "?",
                };
                println!("{kind} {:>10}  {}", e.inode, e.name);
            }
        }
        Cmd::Extract {
            image,
            path,
            out,
            partition,
        } => {
            let (off, size) = resolve_partition(&image, &partition)?;
            let mut fs = Ext4::open(&image, off, size, false)?;
            let inode_data = fs.read_file(&path)?;
            std::fs::write(&out, &inode_data)?;
            println!(
                "wrote {} ({})",
                out.display(),
                human_bytes(inode_data.len() as u64)
            );
        }
        Cmd::Unsparse { input, output } => {
            let n = sparse::sparse_to_raw(&input, &output, None)?;
            println!("raw image: {} ({})", output.display(), human_bytes(n));
        }
        Cmd::Sparse {
            input,
            output,
            block_size,
        } => {
            sparse::raw_to_sparse(&input, &output, block_size, None)?;
            println!("sparse image: {}", output.display());
        }
        Cmd::Replace {
            image,
            path,
            src,
            partition,
        } => {
            let (off, size) = resolve_partition(&image, &partition)?;
            let data = std::fs::read(&src).with_context(|| format!("read {}", src.display()))?;
            let mut fs = Ext4::open(&image, off, size, true)?;
            let ino_before = fs.lookup(&path)?;
            let before = fs.selinux_context(ino_before).ok().flatten();
            fs.replace_file(&path, &data)?;
            let ino_after = fs.lookup(&path)?;
            let after = fs.selinux_context(ino_after).ok().flatten();
            println!(
                "replaced {path} ({}); selinux before={:?} after={:?}",
                human_bytes(data.len() as u64),
                before,
                after
            );
        }
        Cmd::SyncTree {
            image,
            dest,
            src,
            partition,
        } => {
            let (off, size) = resolve_partition(&image, &partition)?;
            let mut fs = Ext4::open(&image, off, size, true)?;
            fs.sync_tree(&dest, &src)?;
            let listed = fs.list_dir(&dest)?;
            println!(
                "synced {dest} <- {} ({} entries)",
                src.display(),
                listed.len()
            );
        }
        Cmd::Unlz4 { input, output } => {
            let n = lz4frame::decompress_file(&input, &output, None)?;
            println!("decompressed: {} ({})", output.display(), human_bytes(n));
        }
        Cmd::Lz4 { input, output } => {
            lz4frame::compress_file(&input, &output, None)?;
            println!("compressed: {}", output.display());
        }
    }
    Ok(())
}
