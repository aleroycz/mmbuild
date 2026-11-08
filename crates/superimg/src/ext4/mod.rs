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

//! A focused ext4 reader/writer for patching an Android `system` partition in
//! place, operating on a byte region `[base .. base+size)` of a larger file
//! (the raw `super` image) so nothing is extracted or loop-mounted.
//!
//! Scope is deliberate — enough to do the two jobs the builder needs:
//!   * read: resolve a path, read a file, read its xattrs (SELinux context),
//!     list a directory, enumerate used blocks/inodes;
//!   * write: replace a file's contents in place (shrinking or growing via the
//!     block bitmap), create a new file in a directory, and delete one.
//!
//! It targets the common Android layout: 4 KiB blocks, extent-mapped inodes
//! (`EXT4_FEATURE_INCOMPAT_EXTENTS`), 256-byte inodes, optional 64-bit feature.
//! Htree-indexed directories are read through their backward-compatible linear
//! entries. Unsupported shapes return a clear error rather than corrupting data.

mod reader;
mod writer;

pub use reader::{DirEntry, Ext4};

use anyhow::{bail, Result};

pub(crate) const SUPERBLOCK_OFFSET: u64 = 1024;
pub(crate) const EXT4_MAGIC: u16 = 0xEF53;
pub(crate) const EXT4_EXTENTS_FL: u32 = 0x8_0000;
pub(crate) const EXTENT_MAGIC: u16 = 0xF30A;
pub(crate) const ROOT_INO: u32 = 2;

// file_type in dir entries
pub(crate) const FT_REG: u8 = 1;
pub(crate) const FT_DIR: u8 = 2;

// i_mode type bits
pub(crate) const S_IFREG: u16 = 0x8000;
pub(crate) const S_IFDIR: u16 = 0x4000;

/// Parsed superblock geometry we care about.
#[derive(Clone, Debug)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    pub first_data_block: u32,
    pub desc_size: u16,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub is_64bit: bool,
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    /// 16-byte filesystem UUID (seed for gdt_csum crc16).
    pub uuid: [u8; 16],
    /// First non-reserved inode (s_first_ino; 11 on modern ext4).
    pub first_ino: u32,
    /// `gdt_csum` (uninit_bg + crc16 group-descriptor checksums) enabled.
    pub gdt_csum: bool,
}

impl Superblock {
    pub(crate) fn group_count(&self) -> u32 {
        ((self.blocks_count - self.first_data_block as u64 + self.blocks_per_group as u64 - 1)
            / self.blocks_per_group as u64) as u32
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.block_size == 0 || self.inodes_per_group == 0 || self.blocks_per_group == 0 {
            bail!("ext4 superblock has zero geometry");
        }
        Ok(())
    }
}
