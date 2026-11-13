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

//! ext4 in-place mutation: extents, flex_bg, gdt_csum, no metadata_csum.

use anyhow::{bail, Context, Result};

use super::reader::Ext4;
use super::*;

const BG_INODE_UNINIT: u16 = 0x1;
const BG_BLOCK_UNINIT: u16 = 0x2;

const EXTENT_HEADER_MAGIC: u16 = EXTENT_MAGIC;

/// Kernel `crc16` (poly 0xA001, reflected), used for gdt_csum.
fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn wr_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

impl Ext4 {
    pub fn has_metadata_csum(&self) -> bool {
        self.superblock().feature_ro_compat & 0x400 != 0
    }
    pub fn has_64bit(&self) -> bool {
        self.superblock().is_64bit
    }

    // ---- geometry shortcuts ----
    fn bs(&self) -> u64 {
        self.sb.block_size as u64
    }
    fn fdb(&self) -> u64 {
        self.sb.first_data_block as u64
    }
    fn bpg(&self) -> u64 {
        self.sb.blocks_per_group as u64
    }

    // ---- group descriptor read / write (+ crc16) ----
    fn read_gdesc(&mut self, g: usize) -> Result<Vec<u8>> {
        let off = self.gd[g].self_offset;
        let mut d = vec![0u8; self.sb.desc_size as usize];
        self.read_exact_at(off, &mut d)?;
        Ok(d)
    }
    fn write_gdesc(&mut self, g: usize, mut d: Vec<u8>) -> Result<()> {
        if self.sb.gdt_csum {
            let mut crc = crc16_update(0xFFFF, &self.sb.uuid);
            crc = crc16_update(crc, &(g as u32).to_le_bytes());
            crc = crc16_update(crc, &d[0..30]);
            // 32-bit desc: checksum field at offset 30, nothing after it.
            d[30] = (crc & 0xff) as u8;
            d[31] = (crc >> 8) as u8;
        }
        let off = self.gd[g].self_offset;
        self.write_all_at(off, &d)
    }

    // ---- superblock free counters ----
    fn adjust_sb_free_blocks(&mut self, delta: i64) -> Result<()> {
        let mut lo = [0u8; 4];
        self.read_exact_at(SUPERBLOCK_OFFSET + 12, &mut lo)?;
        let cur = u32::from_le_bytes(lo) as i64;
        let new = (cur + delta).max(0) as u32;
        self.write_all_at(SUPERBLOCK_OFFSET + 12, &new.to_le_bytes())?;
        Ok(())
    }
    fn adjust_sb_free_inodes(&mut self, delta: i64) -> Result<()> {
        let mut lo = [0u8; 4];
        self.read_exact_at(SUPERBLOCK_OFFSET + 16, &mut lo)?;
        let cur = u32::from_le_bytes(lo) as i64;
        let new = (cur + delta).max(0) as u32;
        self.write_all_at(SUPERBLOCK_OFFSET + 16, &new.to_le_bytes())?;
        Ok(())
    }

    fn group_block_count(&self, g: usize) -> u64 {
        let groups = self.sb.group_count() as u64;
        if (g as u64) < groups - 1 {
            self.bpg()
        } else {
            self.sb.blocks_count - self.fdb() - (groups - 1) * self.bpg()
        }
    }

    // ---- block allocation / free ----

    /// Allocate `count` blocks, preferring a single contiguous run.
    fn alloc_blocks(&mut self, count: u64) -> Result<Vec<u64>> {
        if count == 0 {
            return Ok(vec![]);
        }
        if let Some(start) = self.alloc_contiguous(count)? {
            return Ok((start..start + count).collect());
        }
        // Fall back to scattered single-block allocation.
        let mut out = Vec::with_capacity(count as usize);
        let groups = self.sb.group_count() as usize;
        for g in 0..groups {
            if out.len() as u64 == count {
                break;
            }
            let d = self.read_gdesc(g)?;
            if rd_u16(&d, 18) & BG_BLOCK_UNINIT != 0 {
                continue;
            }
            let free = rd_u16(&d, 12) as u64; // 32-bit path: free_blocks lo only
            if free == 0 {
                continue;
            }
            let bm_block = self.gd[g].block_bitmap;
            let mut bm = self.read_block(bm_block)?;
            let gblocks = self.group_block_count(g);
            let mut taken = 0u64;
            for bit in 0..gblocks as usize {
                if out.len() as u64 == count {
                    break;
                }
                if !bit_get(&bm, bit) {
                    bit_set(&mut bm, bit, true);
                    out.push(self.fdb() + g as u64 * self.bpg() + bit as u64);
                    taken += 1;
                }
            }
            if taken > 0 {
                self.write_block(bm_block, &bm)?;
                let mut d2 = d;
                wr_u16(&mut d2, 12, (free - taken) as u16);
                self.write_gdesc(g, d2)?;
                self.adjust_sb_free_blocks(-(taken as i64))?;
            }
        }
        if out.len() as u64 != count {
            bail!(
                "out of free blocks in initialized groups (needed {count}, got {})",
                out.len()
            );
        }
        out.sort_unstable();
        Ok(out)
    }

    fn alloc_contiguous(&mut self, count: u64) -> Result<Option<u64>> {
        let groups = self.sb.group_count() as usize;
        for g in 0..groups {
            let d = self.read_gdesc(g)?;
            if rd_u16(&d, 18) & BG_BLOCK_UNINIT != 0 {
                continue;
            }
            let free = rd_u16(&d, 12) as u64;
            if free < count {
                continue;
            }
            let bm_block = self.gd[g].block_bitmap;
            let mut bm = self.read_block(bm_block)?;
            let gblocks = self.group_block_count(g);
            let mut run_start: Option<usize> = None;
            let mut found: Option<usize> = None;
            for bit in 0..gblocks as usize {
                if bit_get(&bm, bit) {
                    run_start = None;
                } else {
                    let s = *run_start.get_or_insert(bit);
                    if (bit - s + 1) as u64 == count {
                        found = Some(s);
                        break;
                    }
                }
            }
            if let Some(s) = found {
                for bit in s..s + count as usize {
                    bit_set(&mut bm, bit, true);
                }
                self.write_block(bm_block, &bm)?;
                let mut d2 = d;
                wr_u16(&mut d2, 12, (free - count) as u16);
                self.write_gdesc(g, d2)?;
                self.adjust_sb_free_blocks(-(count as i64))?;
                return Ok(Some(self.fdb() + g as u64 * self.bpg() + s as u64));
            }
        }
        Ok(None)
    }

    fn free_blocks(&mut self, blocks: &[u64]) -> Result<()> {
        // Group blocks by group to minimize bitmap read/writes.
        let mut by_group: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
        for &b in blocks {
            let g = ((b - self.fdb()) / self.bpg()) as usize;
            let bit = ((b - self.fdb()) % self.bpg()) as usize;
            by_group.entry(g).or_default().push(bit);
        }
        for (g, bits) in by_group {
            let bm_block = self.gd[g].block_bitmap;
            let mut bm = self.read_block(bm_block)?;
            let mut freed = 0i64;
            for bit in bits {
                if bit_get(&bm, bit) {
                    bit_set(&mut bm, bit, false);
                    freed += 1;
                }
            }
            self.write_block(bm_block, &bm)?;
            let mut d = self.read_gdesc(g)?;
            let cur = rd_u16(&d, 12) as i64;
            wr_u16(&mut d, 12, (cur + freed) as u16);
            self.write_gdesc(g, d)?;
            self.adjust_sb_free_blocks(freed)?;
        }
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &[u8]) -> Result<()> {
        let off = self.block_off(block);
        self.write_all_at(off, data)
    }

    // ---- inode allocation / free ----

    /// Initialize uninit_bg inode metadata and inode bitmap for a new group.
    fn init_inode_group(&mut self, g: usize, d: &mut [u8]) -> Result<()> {
        let ipg = self.sb.inodes_per_group as usize;
        let bs = self.sb.block_size as usize;
        let mut bm = vec![0u8; bs];
        for bit in ipg..(bs * 8) {
            bit_set(&mut bm, bit, true);
        }
        let bm_block = self.gd[g].inode_bitmap;
        self.write_block(bm_block, &bm)?;
        // Zero the whole inode table for the group.
        let itable_bytes = ipg * self.sb.inode_size as usize;
        let itable_blocks = itable_bytes.div_ceil(bs) as u64;
        let base = self.gd[g].inode_table;
        let zero = vec![0u8; bs];
        for i in 0..itable_blocks {
            self.write_block(base + i, &zero)?;
        }
        let flags = rd_u16(d, 18) & !BG_INODE_UNINIT;
        wr_u16(d, 18, flags);
        wr_u16(d, 28, 0); // bg_itable_unused_lo
        Ok(())
    }

    fn alloc_inode(&mut self, want_dir: bool) -> Result<u32> {
        let groups = self.sb.group_count() as usize;
        let ipg = self.sb.inodes_per_group as usize;
        for g in 0..groups {
            let mut d = self.read_gdesc(g)?;
            let free = rd_u16(&d, 14) as u64;
            if free == 0 {
                continue;
            }
            // The free inodes may live in a still-uninitialized group; lay down its bitmap/table so
            // we can allocate from it.
            if rd_u16(&d, 18) & BG_INODE_UNINIT != 0 {
                self.init_inode_group(g, &mut d)?;
            }
            let bm_block = self.gd[g].inode_bitmap;
            let mut bm = self.read_block(bm_block)?;
            for bit in 0..ipg {
                let ino = (g * ipg + bit + 1) as u32;
                if ino < self.sb.first_ino && ino != ROOT_INO {
                    continue;
                }
                if !bit_get(&bm, bit) {
                    bit_set(&mut bm, bit, true);
                    self.write_block(bm_block, &bm)?;
                    let mut d2 = d;
                    wr_u16(&mut d2, 14, (free - 1) as u16);
                    if want_dir {
                        let used_dirs = rd_u16(&d2, 16);
                        wr_u16(&mut d2, 16, used_dirs + 1);
                    }
                    self.write_gdesc(g, d2)?;
                    self.adjust_sb_free_inodes(-1)?;
                    // Clear the inode slot.
                    let off = self.inode_offset(ino)?;
                    let zero = vec![0u8; self.sb.inode_size as usize];
                    self.write_all_at(off, &zero)?;
                    return Ok(ino);
                }
            }
        }
        bail!("out of free inodes (filesystem full)");
    }

    fn free_inode(&mut self, ino: u32, is_dir: bool) -> Result<()> {
        let ipg = self.sb.inodes_per_group as usize;
        let g = ((ino - 1) as usize) / ipg;
        let bit = ((ino - 1) as usize) % ipg;
        let bm_block = self.gd[g].inode_bitmap;
        let mut bm = self.read_block(bm_block)?;
        if bit_get(&bm, bit) {
            bit_set(&mut bm, bit, false);
            self.write_block(bm_block, &bm)?;
            let mut d = self.read_gdesc(g)?;
            let free = rd_u16(&d, 14);
            wr_u16(&mut d, 14, free + 1);
            if is_dir {
                let used = rd_u16(&d, 16);
                wr_u16(&mut d, 16, used.saturating_sub(1));
            }
            self.write_gdesc(g, d)?;
            self.adjust_sb_free_inodes(1)?;
        }
        // Zero the inode (set dtime).
        let off = self.inode_offset(ino)?;
        let zero = vec![0u8; self.sb.inode_size as usize];
        self.write_all_at(off, &zero)?;
        Ok(())
    }

    // ---- extent tree (depth 0, inline) ----

    /// Build an inline extent tree (header + leaves) into a 60-byte i_block buffer from a sorted
    /// list of physical blocks.
    fn build_inline_extents(blocks: &[u64]) -> Result<[u8; 60]> {
        let mut runs: Vec<(u32, u64, u32)> = Vec::new(); // (logical, phys_start, len)
        let mut logical = 0u32;
        let mut i = 0usize;
        while i < blocks.len() {
            let start = blocks[i];
            let mut len = 1u32;
            while i + (len as usize) < blocks.len()
                && blocks[i + len as usize] == start + len as u64
            {
                len += 1;
            }
            runs.push((logical, start, len));
            logical += len;
            i += len as usize;
        }
        if runs.len() > 4 {
            bail!(
                "file needs {} extents; inline tree holds 4 (index blocks unimplemented)",
                runs.len()
            );
        }
        let mut ib = [0u8; 60];
        wr_u16(&mut ib, 0, EXTENT_HEADER_MAGIC);
        wr_u16(&mut ib, 2, runs.len() as u16); // eh_entries
        wr_u16(&mut ib, 4, 4); // eh_max
        wr_u16(&mut ib, 6, 0); // eh_depth
        wr_u32(&mut ib, 8, 0); // eh_generation
        for (k, (lg, phys, len)) in runs.iter().enumerate() {
            let o = 12 + k * 12;
            wr_u32(&mut ib, o, *lg); // ee_block
            wr_u16(&mut ib, o + 4, *len as u16); // ee_len (written/initialized)
            wr_u16(&mut ib, o + 6, (*phys >> 32) as u16); // ee_start_hi
            wr_u32(&mut ib, o + 8, (*phys & 0xffff_ffff) as u32); // ee_start_lo
        }
        Ok(ib)
    }

    // ---- public: replace a regular file's contents in place ----

    /// Overwrite the contents of the regular file at `path`, preserving its inode number and
    /// therefore its SELinux context.
    pub fn replace_file(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let ino = self
            .lookup(path)
            .with_context(|| format!("replace_file: {path}"))?;
        let inode = self.read_inode(ino)?;
        if !inode.is_reg() {
            bail!("{path} is not a regular file");
        }
        let old_leaves = self.inode_extents(&inode)?;
        let mut old_blocks = Vec::new();
        for lf in &old_leaves {
            for k in 0..lf.len as u64 {
                old_blocks.push(lf.phys + k);
            }
        }
        // Free first so the allocator can reuse the space (and stay contiguous).
        self.free_blocks(&old_blocks)?;

        let bs = self.bs();
        let nblocks = (data.len() as u64).div_ceil(bs);
        let new_blocks = self.alloc_blocks(nblocks)?;
        // Write data.
        for (i, &blk) in new_blocks.iter().enumerate() {
            let start = i as u64 * bs;
            let end = ((start + bs).min(data.len() as u64)) as usize;
            let mut buf = vec![0u8; bs as usize];
            buf[..end - start as usize].copy_from_slice(&data[start as usize..end]);
            self.write_block(blk, &buf)?;
        }
        // Rebuild inode.
        let mut raw = inode.raw.clone();
        wr_u32(&mut raw, 4, (data.len() as u64 & 0xffff_ffff) as u32); // i_size_lo
        wr_u32(&mut raw, 108, (data.len() as u64 >> 32) as u32); // i_size_high
        wr_u32(&mut raw, 28, (nblocks * (bs / 512)) as u32); // i_blocks_lo (512B sectors)
                                                             // i_flags: ensure EXTENTS set.
        let flags = u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]) | EXT4_EXTENTS_FL;
        wr_u32(&mut raw, 32, flags);
        // Rewrite i_block extent area.
        for b in &mut raw[40..100] {
            *b = 0;
        }
        let ib = Self::build_inline_extents(&new_blocks)?;
        raw[40..100].copy_from_slice(&ib);
        let off = self.inode_offset(ino)?;
        self.write_all_at(off, &raw)?;
        Ok(())
    }

    // ---- inode block-list helpers (dirs + files) ----

    fn inode_block_list(&mut self, inode: &super::reader::Inode) -> Result<Vec<u64>> {
        let leaves = self.inode_extents(inode)?;
        let mut blocks = Vec::new();
        for lf in &leaves {
            for k in 0..lf.len as u64 {
                blocks.push(lf.phys + k);
            }
        }
        Ok(blocks)
    }

    /// Rewrite an inode's inline extents + size + i_blocks from a block list.
    fn set_inode_blocks(&mut self, ino: u32, blocks: &[u64], size: u64) -> Result<()> {
        let inode = self.read_inode(ino)?;
        let mut raw = inode.raw.clone();
        wr_u32(&mut raw, 4, (size & 0xffff_ffff) as u32);
        if raw_is_reg(&raw) {
            wr_u32(&mut raw, 108, (size >> 32) as u32);
        }
        wr_u32(
            &mut raw,
            28,
            (blocks.len() as u64 * (self.bs() / 512)) as u32,
        );
        let flags = u32::from_le_bytes([raw[32], raw[33], raw[34], raw[35]]) | EXT4_EXTENTS_FL;
        wr_u32(&mut raw, 32, flags);
        for b in &mut raw[40..100] {
            *b = 0;
        }
        let ib = Self::build_inline_extents(blocks)?;
        raw[40..100].copy_from_slice(&ib);
        let off = self.inode_offset(ino)?;
        self.write_all_at(off, &raw)
    }

    // ---- directory entry management ----

    fn dir_add_entry(&mut self, dir_ino: u32, name: &str, child_ino: u32, ftype: u8) -> Result<()> {
        if name.len() > 255 {
            bail!("dir entry name too long: {name}");
        }
        let need = round4(8 + name.len());
        let bs = self.bs() as usize;
        let dir_inode = self.read_inode(dir_ino)?;
        let blocks = self.inode_block_list(&dir_inode)?;

        for &blk in &blocks {
            let mut data = self.read_block(blk)?;
            let mut pos = 0usize;
            while pos + 8 <= bs {
                let ino =
                    u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                let rec_len = rd_u16(&data, pos + 4) as usize;
                let name_len = data[pos + 6] as usize;
                if rec_len < 8 {
                    break;
                }
                let used = if ino == 0 { 0 } else { round4(8 + name_len) };
                if rec_len - used >= need {
                    // Split: shrink current, write new entry in the freed tail.
                    let new_pos = pos + used;
                    let new_rec = rec_len - used;
                    if ino != 0 {
                        wr_u16(&mut data, pos + 4, used as u16);
                    }
                    let at = if ino == 0 { pos } else { new_pos };
                    let rl = if ino == 0 { rec_len } else { new_rec };
                    wr_u32(&mut data, at, child_ino);
                    wr_u16(&mut data, at + 4, rl as u16);
                    data[at + 6] = name.len() as u8;
                    data[at + 7] = ftype;
                    data[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
                    self.write_block(blk, &data)?;
                    return Ok(());
                }
                pos += rec_len;
            }
        }

        // No room - append a fresh directory block.
        let new_blk = self.alloc_blocks(1)?[0];
        let mut data = vec![0u8; bs];
        wr_u32(&mut data, 0, child_ino);
        wr_u16(&mut data, 4, bs as u16);
        data[6] = name.len() as u8;
        data[7] = ftype;
        data[8..8 + name.len()].copy_from_slice(name.as_bytes());
        self.write_block(new_blk, &data)?;
        let mut all = blocks;
        all.push(new_blk);
        let new_size = all.len() as u64 * self.bs();
        self.set_inode_blocks(dir_ino, &all, new_size)?;
        Ok(())
    }

    fn dir_remove_entry(&mut self, dir_ino: u32, name: &str) -> Result<bool> {
        let bs = self.bs() as usize;
        let dir_inode = self.read_inode(dir_ino)?;
        let blocks = self.inode_block_list(&dir_inode)?;
        for &blk in &blocks {
            let mut data = self.read_block(blk)?;
            let mut pos = 0usize;
            let mut prev: Option<usize> = None;
            while pos + 8 <= bs {
                let ino =
                    u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                let rec_len = rd_u16(&data, pos + 4) as usize;
                let name_len = data[pos + 6] as usize;
                if rec_len < 8 {
                    break;
                }
                if ino != 0
                    && name_len == name.len()
                    && &data[pos + 8..pos + 8 + name_len] == name.as_bytes()
                {
                    match prev {
                        Some(p) => {
                            // Merge this record into the previous one.
                            let prev_rec = rd_u16(&data, p + 4) as usize;
                            wr_u16(&mut data, p + 4, (prev_rec + rec_len) as u16);
                        }
                        None => {
                            // First entry: zero the inode field only.
                            wr_u32(&mut data, pos, 0);
                        }
                    }
                    self.write_block(blk, &data)?;
                    return Ok(true);
                }
                prev = Some(pos);
                pos += rec_len;
            }
        }
        Ok(false)
    }

    // ---- create / delete ----

    fn selinux_or_inherit(&mut self, parent_ino: u32, ctx: Option<&str>) -> Result<String> {
        if let Some(c) = ctx {
            return Ok(c.to_string());
        }
        if let Some(c) = self.selinux_context(parent_ino)? {
            return Ok(c);
        }
        Ok("u:object_r:system_file:s0".to_string())
    }

    /// Create a regular file `name` under directory inode `parent_ino`.
    pub fn create_file_in(
        &mut self,
        parent_ino: u32,
        name: &str,
        data: &[u8],
        ctx: Option<&str>,
    ) -> Result<u32> {
        let selinux = self.selinux_or_inherit(parent_ino, ctx)?;
        let bs = self.bs();
        let nblocks = (data.len() as u64).div_ceil(bs);
        let blocks = self.alloc_blocks(nblocks)?;
        for (i, &blk) in blocks.iter().enumerate() {
            let start = i as u64 * bs;
            let end = ((start + bs).min(data.len() as u64)) as usize;
            let mut buf = vec![0u8; bs as usize];
            buf[..end - start as usize].copy_from_slice(&data[start as usize..end]);
            self.write_block(blk, &buf)?;
        }
        let ino = self.alloc_inode(false)?;
        let mut raw = vec![0u8; self.sb.inode_size as usize];
        wr_u16(&mut raw, 0, S_IFREG | 0o644);
        wr_u32(&mut raw, 4, (data.len() as u64 & 0xffff_ffff) as u32);
        wr_u32(&mut raw, 108, (data.len() as u64 >> 32) as u32);
        wr_u16(&mut raw, 26, 1); // links_count
        wr_u32(&mut raw, 28, (nblocks * (bs / 512)) as u32);
        wr_u32(&mut raw, 32, EXT4_EXTENTS_FL);
        let ib = Self::build_inline_extents(&blocks)?;
        raw[40..100].copy_from_slice(&ib);
        set_inline_selinux(&mut raw, &selinux);
        let off = self.inode_offset(ino)?;
        self.write_all_at(off, &raw)?;
        self.dir_add_entry(parent_ino, name, ino, FT_REG)?;
        Ok(ino)
    }

    /// Set the permission bits of an existing regular file (keeps S_IFREG).
    pub fn chmod_reg(&mut self, path: &str, perm: u16) -> Result<()> {
        let ino = self
            .lookup(path)
            .with_context(|| format!("chmod_reg: {path}"))?;
        let off = self.inode_offset(ino)?;
        let mut m = [0u8; 2];
        wr_u16(&mut m, 0, S_IFREG | perm);
        self.write_all_at(off, &m)?;
        Ok(())
    }

    /// Create a subdirectory `name` under directory inode `parent_ino`.
    pub fn mkdir_in(&mut self, parent_ino: u32, name: &str, ctx: Option<&str>) -> Result<u32> {
        let selinux = self.selinux_or_inherit(parent_ino, ctx)?;
        let bs = self.bs();
        let blk = self.alloc_blocks(1)?[0];
        let ino = self.alloc_inode(true)?;
        // "." and ".." in the fresh block.
        let mut data = vec![0u8; bs as usize];
        wr_u32(&mut data, 0, ino);
        wr_u16(&mut data, 4, 12);
        data[6] = 1;
        data[7] = FT_DIR;
        data[8] = b'.';
        wr_u32(&mut data, 12, parent_ino);
        wr_u16(&mut data, 16, (bs - 12) as u16);
        data[18] = 2;
        data[19] = FT_DIR;
        data[20] = b'.';
        data[21] = b'.';
        self.write_block(blk, &data)?;

        let mut raw = vec![0u8; self.sb.inode_size as usize];
        wr_u16(&mut raw, 0, S_IFDIR | 0o755);
        wr_u32(&mut raw, 4, bs as u32); // size = 1 block
        wr_u16(&mut raw, 26, 2); // links_count: "." + parent entry
        wr_u32(&mut raw, 28, (bs / 512) as u32);
        wr_u32(&mut raw, 32, EXT4_EXTENTS_FL);
        let ib = Self::build_inline_extents(&[blk])?;
        raw[40..100].copy_from_slice(&ib);
        set_inline_selinux(&mut raw, &selinux);
        let off = self.inode_offset(ino)?;
        self.write_all_at(off, &raw)?;

        self.dir_add_entry(parent_ino, name, ino, FT_DIR)?;
        // Parent gains a link from the new subdir's "..".
        self.bump_links(parent_ino, 1)?;
        Ok(ino)
    }

    fn bump_links(&mut self, ino: u32, delta: i32) -> Result<()> {
        let off = self.inode_offset(ino)?;
        let mut two = [0u8; 2];
        self.read_exact_at(off + 26, &mut two)?;
        let cur = u16::from_le_bytes(two) as i32;
        let new = (cur + delta).max(0) as u16;
        self.write_all_at(off + 26, &new.to_le_bytes())?;
        Ok(())
    }

    /// Recursively free an inode's data blocks and all descendants.
    fn free_tree(&mut self, ino: u32) -> Result<()> {
        let inode = self.read_inode(ino)?;
        let is_dir = inode.is_dir();
        if is_dir {
            let children = self.read_dir_inode(&inode)?;
            for c in children {
                self.free_tree(c.inode)?;
            }
        }
        let blocks = self.inode_block_list(&inode)?;
        self.free_blocks(&blocks)?;
        self.free_inode(ino, is_dir)?;
        Ok(())
    }

    /// Delete the file or subtree at `path` (removing it from its parent).
    pub fn remove_path(&mut self, path: &str) -> Result<()> {
        let (parent, name) = split_parent(path)?;
        let parent_ino = self.lookup(&parent)?;
        let target_ino = self.lookup(path)?;
        let target = self.read_inode(target_ino)?;
        let is_dir = target.is_dir();
        self.free_tree(target_ino)?;
        self.dir_remove_entry(parent_ino, name)?;
        if is_dir {
            // Parent loses the link from the removed subdir's "..".
            self.bump_links(parent_ino, -1)?;
        }
        Ok(())
    }

    /// Sync local dir into dest_dir (replace; inherit SELinux).
    pub fn sync_tree(&mut self, dest_dir: &str, local_root: &std::path::Path) -> Result<()> {
        if self.lookup(dest_dir).is_ok() {
            self.remove_path(dest_dir)?;
        }
        let (parent, name) = split_parent(dest_dir)?;
        let parent_ino = self.lookup(&parent)?;
        let dir_ino = self.mkdir_in(parent_ino, name, None)?;
        self.copy_into(dir_ino, local_root)?;
        Ok(())
    }

    fn copy_into(&mut self, dir_ino: u32, local: &std::path::Path) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(local)
            .with_context(|| format!("read_dir {}", local.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            let ft = e.file_type()?;
            if ft.is_dir() {
                let sub = self.mkdir_in(dir_ino, &name, None)?;
                self.copy_into(sub, &e.path())?;
            } else if ft.is_file() {
                let data = std::fs::read(e.path())?;
                self.create_file_in(dir_ino, &name, &data, None)?;
            }
        }
        Ok(())
    }
}

fn raw_is_reg(raw: &[u8]) -> bool {
    u16::from_le_bytes([raw[0], raw[1]]) & 0xF000 == S_IFREG
}

fn round4(n: usize) -> usize {
    (n + 3) & !3
}

fn split_parent(path: &str) -> Result<(String, &str)> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Ok(("/".to_string(), &trimmed[1..])),
        Some(i) => Ok((trimmed[..i].to_string(), &trimmed[i + 1..])),
        None => bail!("path has no parent: {path}"),
    }
}

/// Write a single inline `security.selinux` xattr into a fresh inode buffer.
fn set_inline_selinux(raw: &mut [u8], ctx: &str) {
    let inode_size = raw.len();
    let extra = 32usize;
    wr_u16(raw, 128, extra as u16); // i_extra_isize
    let base = 128 + extra; // magic position
    wr_u32(raw, base, 0xEA02_0000); // h_magic
    let ifirst = base + 4;
    // value (NUL-terminated), placed at the top of the inode.
    let mut val = ctx.as_bytes().to_vec();
    val.push(0);
    let valigned = round4(val.len());
    let vpos = inode_size - valigned;
    raw[vpos..vpos + val.len()].copy_from_slice(&val);
    let value_offs = vpos - ifirst;
    // entry
    let e = ifirst;
    raw[e] = 7; // e_name_len "selinux"
    raw[e + 1] = 6; // e_name_index = security
    wr_u16(raw, e + 2, value_offs as u16);
    wr_u32(raw, e + 4, 0); // e_value_inum
    wr_u32(raw, e + 8, val.len() as u32); // e_value_size
    wr_u32(raw, e + 12, 0); // e_hash
    raw[e + 16..e + 16 + 7].copy_from_slice(b"selinux");
    // terminator (zero entry) after the aligned entry.
    let term = e + round4(16 + 7);
    wr_u32(raw, term, 0);
}

// ---- bit helpers (ext4 bitmaps are little-endian bit order) ----
fn bit_get(bm: &[u8], bit: usize) -> bool {
    bm[bit / 8] & (1 << (bit % 8)) != 0
}
fn bit_set(bm: &mut [u8], bit: usize, val: bool) {
    if val {
        bm[bit / 8] |= 1 << (bit % 8);
    } else {
        bm[bit / 8] &= !(1 << (bit % 8));
    }
}
