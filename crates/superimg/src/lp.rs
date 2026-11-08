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

//! Android dynamic-partitions (LP / liblp) metadata reader.
//!
//! A `super` image starts with:
//!   `[4096 reserved][4096 primary geometry][4096 backup geometry][metadata…]`
//! The metadata names each logical partition (`system`, `vendor`, `product`, …)
//! and the physical extents backing it. We parse it to learn, **from the image
//! itself**, where the `system` partition lives — its byte offset and size —
//! instead of hardcoding them. See AOSP `fs_mgr/liblp/include/liblp/metadata_format.h`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

const GEOMETRY_MAGIC: u32 = 0x616c_4467;
const HEADER_MAGIC: u32 = 0x414C_5030;
const PARTITION_RESERVED_BYTES: u64 = 4096;
const GEOMETRY_SIZE: u64 = 4096;
const SECTOR: u64 = 512;
const TARGET_TYPE_LINEAR: u32 = 0;

/// One physical extent backing a logical partition.
#[derive(Clone, Debug)]
pub struct Extent {
    pub num_sectors: u64,
    pub target_type: u32,
    /// First sector on the target device (for LINEAR extents).
    pub target_data: u64,
    pub target_source: u32,
}

/// A logical partition and where it physically lives inside `super`.
#[derive(Clone, Debug)]
pub struct Partition {
    pub name: String,
    pub extents: Vec<Extent>,
}

impl Partition {
    /// Total size in bytes across all extents.
    pub fn size_bytes(&self) -> u64 {
        self.extents.iter().map(|e| e.num_sectors * SECTOR).sum()
    }
    /// Byte offset of the partition's first extent within the super image.
    pub fn first_offset(&self) -> Option<u64> {
        self.extents.first().map(|e| e.target_data * SECTOR)
    }
    /// True if the partition is a single contiguous LINEAR extent — the case we
    /// can safely edit in place.
    pub fn is_contiguous(&self) -> bool {
        self.extents.len() == 1 && self.extents[0].target_type == TARGET_TYPE_LINEAR
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    pub logical_block_size: u32,
    pub partitions: Vec<Partition>,
}

impl Metadata {
    pub fn partition(&self, name: &str) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.name == name)
    }
    /// Find `name`, or `name_a` (slot A) as a fallback.
    pub fn partition_or_slot_a(&self, name: &str) -> Option<&Partition> {
        self.partition(name)
            .or_else(|| self.partition(&format!("{name}_a")))
    }
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

/// Parse the primary metadata slot of a raw (non-sparse) super image.
pub fn read_metadata(path: &Path) -> Result<Metadata> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;

    // --- geometry (primary, at offset 4096) ---
    let mut geo = [0u8; GEOMETRY_SIZE as usize];
    f.seek(SeekFrom::Start(PARTITION_RESERVED_BYTES))?;
    f.read_exact(&mut geo)?;
    if rd_u32(&geo, 0) != GEOMETRY_MAGIC {
        bail!("LP geometry magic not found — is this a raw super image? (try de-sparsing first)");
    }
    let metadata_max_size = rd_u32(&geo, 40); // after magic(4)+struct_size(4)+checksum(32)
    let metadata_slot_count = rd_u32(&geo, 44);
    let logical_block_size = rd_u32(&geo, 48);
    let _ = (metadata_max_size, metadata_slot_count);

    // --- metadata header (primary slot 0) ---
    let metadata_offset = PARTITION_RESERVED_BYTES + GEOMETRY_SIZE * 2;
    let mut hdr = [0u8; 128];
    f.seek(SeekFrom::Start(metadata_offset))?;
    f.read_exact(&mut hdr)?;
    if rd_u32(&hdr, 0) != HEADER_MAGIC {
        bail!("LP metadata header magic not found at offset {metadata_offset}");
    }
    let header_size = rd_u32(&hdr, 8) as u64;

    // Table descriptors begin at byte 80: partitions, extents, groups, devices.
    // descriptor = { offset:u32, num_entries:u32, entry_size:u32 }
    let part_off = rd_u32(&hdr, 80) as u64;
    let part_num = rd_u32(&hdr, 84);
    let part_entry = rd_u32(&hdr, 88) as u64;
    let ext_off = rd_u32(&hdr, 92) as u64;
    let ext_num = rd_u32(&hdr, 96);
    let ext_entry = rd_u32(&hdr, 100) as u64;

    let tables_base = metadata_offset + header_size;

    // --- extents table ---
    let mut extents: Vec<Extent> = Vec::with_capacity(ext_num as usize);
    for i in 0..ext_num as u64 {
        let mut e = vec![0u8; ext_entry as usize];
        f.seek(SeekFrom::Start(tables_base + ext_off + i * ext_entry))?;
        f.read_exact(&mut e)?;
        extents.push(Extent {
            num_sectors: rd_u64(&e, 0),
            target_type: rd_u32(&e, 8),
            target_data: rd_u64(&e, 12),
            target_source: rd_u32(&e, 20),
        });
    }

    // --- partitions table ---
    let mut partitions: Vec<Partition> = Vec::with_capacity(part_num as usize);
    for i in 0..part_num as u64 {
        let mut p = vec![0u8; part_entry as usize];
        f.seek(SeekFrom::Start(tables_base + part_off + i * part_entry))?;
        f.read_exact(&mut p)?;
        let name_raw = &p[0..36];
        let name_end = name_raw.iter().position(|&c| c == 0).unwrap_or(36);
        let name = String::from_utf8_lossy(&name_raw[..name_end]).to_string();
        let first_extent_index = rd_u32(&p, 40) as usize;
        let num_extents = rd_u32(&p, 44) as usize;
        let mut pe = Vec::with_capacity(num_extents);
        for k in 0..num_extents {
            if let Some(ex) = extents.get(first_extent_index + k) {
                pe.push(ex.clone());
            }
        }
        partitions.push(Partition { name, extents: pe });
    }

    Ok(Metadata {
        logical_block_size,
        partitions,
    })
}
