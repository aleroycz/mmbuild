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

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::*;

/// A directory entry returned by [`Ext4::list_dir`].
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub inode: u32,
    pub file_type: u8,
}

/// One leaf extent: `len` logical blocks starting at logical block `logical`
/// map to physical blocks starting at `phys`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Leaf {
    pub logical: u32,
    pub len: u32,
    pub phys: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct GroupDesc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    /// Absolute byte offset of this descriptor within the partition, so the
    /// writer can rewrite free counts after allocating.
    pub self_offset: u64,
}

/// A parsed inode plus its raw bytes (so the writer can edit and write back).
#[derive(Clone)]
pub(crate) struct Inode {
    pub ino: u32,
    pub raw: Vec<u8>,
    pub offset: u64, // absolute byte offset of the inode within the partition
    pub mode: u16,
    pub size: u64,
    pub flags: u32,
    pub blocks_lo: u32,
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & 0xF000 == S_IFDIR
    }
    pub fn is_reg(&self) -> bool {
        self.mode & 0xF000 == S_IFREG
    }
    pub fn uses_extents(&self) -> bool {
        self.flags & EXT4_EXTENTS_FL != 0
    }
}

/// An ext4 filesystem mapped onto a region of an open file.
pub struct Ext4 {
    pub(crate) file: std::fs::File,
    pub(crate) base: u64,
    pub(crate) size: u64,
    pub(crate) sb: Superblock,
    pub(crate) gd: Vec<GroupDesc>,
}

impl Ext4 {
    /// Open the ext4 at `[base .. base+size)` of `path`. `writable` controls
    /// whether the underlying file is opened read-write.
    pub fn open(path: &Path, base: u64, size: u64, writable: bool) -> Result<Ext4> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let mut fs = Ext4 {
            file,
            base,
            size,
            sb: Superblock {
                inodes_count: 0,
                blocks_count: 0,
                block_size: 0,
                blocks_per_group: 0,
                inodes_per_group: 0,
                inode_size: 0,
                first_data_block: 0,
                desc_size: 0,
                feature_incompat: 0,
                feature_ro_compat: 0,
                is_64bit: false,
                free_blocks_count: 0,
                free_inodes_count: 0,
                uuid: [0u8; 16],
                first_ino: 11,
                gdt_csum: false,
            },
            gd: Vec::new(),
        };
        fs.parse_superblock()?;
        fs.parse_group_descs()?;
        Ok(fs)
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    // ---- low-level region IO ----
    pub(crate) fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off + buf.len() as u64 > self.size {
            bail!("read past end of partition (off {off}, len {})", buf.len());
        }
        self.file.seek(SeekFrom::Start(self.base + off))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
    pub(crate) fn write_all_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if off + buf.len() as u64 > self.size {
            bail!("write past end of partition (off {off}, len {})", buf.len());
        }
        self.file.seek(SeekFrom::Start(self.base + off))?;
        self.file.write_all(buf)?;
        Ok(())
    }
    pub(crate) fn block_off(&self, block: u64) -> u64 {
        block * self.sb.block_size as u64
    }
    pub(crate) fn read_block(&mut self, block: u64) -> Result<Vec<u8>> {
        let mut b = vec![0u8; self.sb.block_size as usize];
        self.read_exact_at(self.block_off(block), &mut b)?;
        Ok(b)
    }

    fn parse_superblock(&mut self) -> Result<()> {
        let mut sb = [0u8; 1024];
        self.file.seek(SeekFrom::Start(self.base + SUPERBLOCK_OFFSET))?;
        self.file.read_exact(&mut sb)?;
        let magic = u16::from_le_bytes([sb[56], sb[57]]);
        if magic != EXT4_MAGIC {
            bail!("ext4 magic 0xEF53 not found (got {magic:#06x}) — not an ext4 partition");
        }
        let u32a = |o: usize| u32::from_le_bytes([sb[o], sb[o + 1], sb[o + 2], sb[o + 3]]);
        let u16a = |o: usize| u16::from_le_bytes([sb[o], sb[o + 1]]);
        let log_bs = u32a(24);
        let block_size = 1024u32 << log_bs;
        let feature_incompat = u32a(96);
        let is_64bit = feature_incompat & 0x80 != 0;
        let desc_size = {
            let d = u16a(254);
            if is_64bit && d != 0 {
                d
            } else {
                32
            }
        };
        let blocks_lo = u32a(4) as u64;
        let blocks_hi = if is_64bit { u32a(0x150) as u64 } else { 0 };
        let free_blocks_lo = u32a(12) as u64;
        let free_blocks_hi = if is_64bit { u32a(0x158) as u64 } else { 0 };
        self.sb = Superblock {
            inodes_count: u32a(0),
            blocks_count: blocks_lo | (blocks_hi << 32),
            block_size,
            blocks_per_group: u32a(32),
            inodes_per_group: u32a(40),
            inode_size: {
                let s = u16a(88);
                if s == 0 {
                    128
                } else {
                    s
                }
            },
            first_data_block: u32a(20),
            desc_size,
            feature_incompat,
            feature_ro_compat: u32a(100),
            is_64bit,
            free_blocks_count: free_blocks_lo | (free_blocks_hi << 32),
            free_inodes_count: u32a(16),
            uuid: {
                let mut u = [0u8; 16];
                u.copy_from_slice(&sb[104..120]);
                u
            },
            first_ino: {
                let fi = u32a(84);
                if fi == 0 {
                    11
                } else {
                    fi
                }
            },
            gdt_csum: u32a(100) & 0x10 != 0,
        };
        self.sb.validate()
    }

    fn parse_group_descs(&mut self) -> Result<()> {
        let groups = self.sb.group_count();
        let bs = self.sb.block_size as u64;
        // Group descriptor table begins in the block after the superblock.
        let gdt_block = if bs == 1024 { 2 } else { 1 };
        let gdt_off = gdt_block * bs;
        let dsz = self.sb.desc_size as u64;
        let mut gd = Vec::with_capacity(groups as usize);
        for g in 0..groups as u64 {
            let off = gdt_off + g * dsz;
            let mut d = vec![0u8; dsz as usize];
            self.read_exact_at(off, &mut d)?;
            let u32a = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) as u64;
            let hi = |o: usize| -> u64 {
                if self.sb.is_64bit && dsz >= 64 {
                    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) as u64
                } else {
                    0
                }
            };
            gd.push(GroupDesc {
                block_bitmap: u32a(0) | (hi(32) << 32),
                inode_bitmap: u32a(4) | (hi(36) << 32),
                inode_table: u32a(8) | (hi(40) << 32),
                self_offset: off,
            });
        }
        self.gd = gd;
        Ok(())
    }

    // ---- inodes ----
    pub(crate) fn inode_offset(&self, ino: u32) -> Result<u64> {
        if ino == 0 {
            bail!("inode 0 is invalid");
        }
        let ipg = self.sb.inodes_per_group;
        let group = ((ino - 1) / ipg) as usize;
        let index = ((ino - 1) % ipg) as u64;
        let gd = self
            .gd
            .get(group)
            .with_context(|| format!("inode {ino} out of range (group {group})"))?;
        Ok(self.block_off(gd.inode_table) + index * self.sb.inode_size as u64)
    }

    pub(crate) fn read_inode(&mut self, ino: u32) -> Result<Inode> {
        let off = self.inode_offset(ino)?;
        let mut raw = vec![0u8; self.sb.inode_size as usize];
        self.read_exact_at(off, &mut raw)?;
        let u32a = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
        let u16a = |o: usize| u16::from_le_bytes([raw[o], raw[o + 1]]);
        let mode = u16a(0);
        let size_lo = u32a(4) as u64;
        let size_hi = u32a(108) as u64; // i_size_high for regular files
        let size = if mode & 0xF000 == S_IFREG {
            size_lo | (size_hi << 32)
        } else {
            size_lo
        };
        Ok(Inode {
            ino,
            offset: off,
            mode,
            size,
            flags: u32a(32),
            blocks_lo: u32a(28),
            raw,
        })
    }

    /// Collect the leaf extents of an extent-mapped inode (walks index nodes).
    pub(crate) fn inode_extents(&mut self, inode: &Inode) -> Result<Vec<Leaf>> {
        if !inode.uses_extents() {
            bail!(
                "inode {} is not extent-mapped (block-mapped inodes unsupported)",
                inode.ino
            );
        }
        let mut leaves = Vec::new();
        let root = &inode.raw[40..100]; // i_block, 60 bytes
        self.walk_extent_node(root, &mut leaves)?;
        leaves.sort_by_key(|l| l.logical);
        Ok(leaves)
    }

    fn walk_extent_node(&mut self, node: &[u8], out: &mut Vec<Leaf>) -> Result<()> {
        let magic = u16::from_le_bytes([node[0], node[1]]);
        if magic != EXTENT_MAGIC {
            bail!("bad extent header magic {magic:#06x}");
        }
        let entries = u16::from_le_bytes([node[2], node[3]]) as usize;
        let depth = u16::from_le_bytes([node[6], node[7]]);
        if depth == 0 {
            for i in 0..entries {
                let e = &node[12 + i * 12..12 + i * 12 + 12];
                let logical = u32::from_le_bytes([e[0], e[1], e[2], e[3]]);
                let mut len = u16::from_le_bytes([e[4], e[5]]) as u32;
                if len > 32768 {
                    len -= 32768; // unwritten but allocated
                }
                let start_hi = u16::from_le_bytes([e[6], e[7]]) as u64;
                let start_lo = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
                out.push(Leaf {
                    logical,
                    len,
                    phys: (start_hi << 32) | start_lo,
                });
            }
        } else {
            for i in 0..entries {
                let e = &node[12 + i * 12..12 + i * 12 + 12];
                let leaf_lo = u32::from_le_bytes([e[4], e[5], e[6], e[7]]) as u64;
                let leaf_hi = u16::from_le_bytes([e[8], e[9]]) as u64;
                let child_block = (leaf_hi << 32) | leaf_lo;
                let child = self.read_block(child_block)?;
                self.walk_extent_node(&child, out)?;
            }
        }
        Ok(())
    }

    /// Read the full contents of a regular file inode.
    pub fn read_file_inode(&mut self, inode: &Inode) -> Result<Vec<u8>> {
        if !inode.is_reg() {
            bail!("inode {} is not a regular file", inode.ino);
        }
        let bs = self.sb.block_size as u64;
        let leaves = self.inode_extents(inode)?;
        let mut data = vec![0u8; inode.size as usize];
        for lf in leaves {
            for k in 0..lf.len as u64 {
                let logical = lf.logical as u64 + k;
                let file_off = logical * bs;
                if file_off >= inode.size {
                    break;
                }
                let block = self.read_block(lf.phys + k)?;
                let take = ((inode.size - file_off).min(bs)) as usize;
                data[file_off as usize..file_off as usize + take].copy_from_slice(&block[..take]);
            }
        }
        Ok(data)
    }

    /// Iterate a directory inode's linear entries.
    pub(crate) fn read_dir_inode(&mut self, inode: &Inode) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            bail!("inode {} is not a directory", inode.ino);
        }
        let bs = self.sb.block_size as usize;
        let leaves = self.inode_extents(inode)?;
        let mut entries = Vec::new();
        for lf in leaves {
            for k in 0..lf.len as u64 {
                let block = self.read_block(lf.phys + k)?;
                let mut pos = 0usize;
                while pos + 8 <= bs {
                    let ino = u32::from_le_bytes([
                        block[pos],
                        block[pos + 1],
                        block[pos + 2],
                        block[pos + 3],
                    ]);
                    let rec_len =
                        u16::from_le_bytes([block[pos + 4], block[pos + 5]]) as usize;
                    let name_len = block[pos + 6] as usize;
                    let file_type = block[pos + 7];
                    if rec_len < 8 {
                        break;
                    }
                    if ino != 0 && name_len > 0 && pos + 8 + name_len <= bs {
                        let name =
                            String::from_utf8_lossy(&block[pos + 8..pos + 8 + name_len]).to_string();
                        if name != "." && name != ".." {
                            entries.push(DirEntry {
                                name,
                                inode: ino,
                                file_type,
                            });
                        }
                    }
                    pos += rec_len;
                }
            }
        }
        Ok(entries)
    }

    /// Read a regular file by path.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let ino = self.lookup(path)?;
        let inode = self.read_inode(ino)?;
        self.read_file_inode(&inode)
    }

    /// Public directory listing by path.
    pub fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
        let ino = self.lookup(path)?;
        let inode = self.read_inode(ino)?;
        self.read_dir_inode(&inode)
    }

    /// Resolve an absolute path to its inode number.
    pub fn lookup(&mut self, path: &str) -> Result<u32> {
        let mut ino = ROOT_INO;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let inode = self.read_inode(ino)?;
            let entries = self.read_dir_inode(&inode)?;
            let next = entries
                .iter()
                .find(|e| e.name == comp)
                .with_context(|| format!("path component '{comp}' not found in {path}"))?;
            ino = next.inode;
        }
        Ok(ino)
    }

    /// Best-effort read of the inline/external `security.selinux` xattr.
    pub fn selinux_context(&mut self, ino: u32) -> Result<Option<String>> {
        let inode = self.read_inode(ino)?;
        // Inline xattrs: start at 128 + i_extra_isize, begin with magic 0xEA020000.
        if self.sb.inode_size > 128 {
            let extra = u16::from_le_bytes([inode.raw[128], inode.raw[129]]) as usize;
            let start = 128 + extra;
            if start + 4 <= inode.raw.len() {
                let magic = u32::from_le_bytes([
                    inode.raw[start],
                    inode.raw[start + 1],
                    inode.raw[start + 2],
                    inode.raw[start + 3],
                ]);
                if magic == 0xEA02_0000 {
                    let region = &inode.raw[start + 4..];
                    let value_base = start + 4;
                    if let Some(v) = find_selinux_entry(region, &inode.raw, value_base) {
                        return Ok(Some(v));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Scan a block of xattr entries for `security.selinux` and return its value.
fn find_selinux_entry(entries: &[u8], container: &[u8], value_base: usize) -> Option<String> {
    let mut pos = 0usize;
    while pos + 16 <= entries.len() {
        let name_len = entries[pos] as usize;
        let name_index = entries[pos + 1];
        if name_len == 0 {
            break; // end marker
        }
        let value_offs = u16::from_le_bytes([entries[pos + 2], entries[pos + 3]]) as usize;
        let value_size =
            u32::from_le_bytes([entries[pos + 8], entries[pos + 9], entries[pos + 10], entries[pos + 11]])
                as usize;
        if pos + 16 + name_len > entries.len() {
            break;
        }
        let name = &entries[pos + 16..pos + 16 + name_len];
        // name_index 6 == "security." namespace.
        if name_index == 6 && name == b"selinux" {
            let vstart = value_base + value_offs;
            if vstart + value_size <= container.len() {
                let raw = &container[vstart..vstart + value_size];
                let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                return Some(String::from_utf8_lossy(&raw[..end]).to_string());
            }
        }
        let entry_len = 16 + name_len;
        pos += (entry_len + 3) & !3; // 4-byte align
    }
    None
}
