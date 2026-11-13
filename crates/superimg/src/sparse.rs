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

//! Streaming Android sparse <-> raw codec (img2simg / simg2img).

#![allow(clippy::needless_option_as_deref)] /* Option<&mut dyn FnMut> needs as_deref_mut */

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

const SPARSE_MAGIC: u32 = 0xED26_FF3A;
const FILE_HDR_SZ: u16 = 28;
const CHUNK_HDR_SZ: u16 = 12;

const CHUNK_RAW: u16 = 0xCAC1;
const CHUNK_FILL: u16 = 0xCAC2;
const CHUNK_DONT_CARE: u16 = 0xCAC3;
const CHUNK_CRC32: u16 = 0xCAC4;

/// Max RAW chunk blocks (u32 total_sz must stay under 4 GiB).
const MAX_RAW_RUN_BLOCKS: u32 = 32768;

/// Progress callback: `(bytes_done, bytes_total)`.
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

/// True if the file begins with the sparse magic.
pub fn is_sparse(path: &Path) -> Result<bool> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut m = [0u8; 4];
    if f.read_exact(&mut m).is_err() {
        return Ok(false);
    }
    Ok(u32::from_le_bytes(m) == SPARSE_MAGIC)
}

fn rd_u16(r: &mut impl Read) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn rd_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Expand a sparse image into a flat raw image.
pub fn sparse_to_raw(input: &Path, output: &Path, mut progress: Option<Progress>) -> Result<u64> {
    let fin = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let total_in = fin.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 20, fin);
    let mut w = BufWriter::with_capacity(1 << 20, File::create(output)?);

    let magic = rd_u32(&mut r)?;
    if magic != SPARSE_MAGIC {
        bail!("not a sparse image (magic {magic:#010x})");
    }
    let _major = rd_u16(&mut r)?;
    let _minor = rd_u16(&mut r)?;
    let file_hdr_sz = rd_u16(&mut r)?;
    let chunk_hdr_sz = rd_u16(&mut r)?;
    let blk_sz = rd_u32(&mut r)? as u64;
    let _total_blks = rd_u32(&mut r)?;
    let total_chunks = rd_u32(&mut r)?;
    let _checksum = rd_u32(&mut r)?;
    // Skip any extra header bytes beyond the 28 we parsed.
    if file_hdr_sz as usize > 28 {
        let mut skip = vec![0u8; file_hdr_sz as usize - 28];
        r.read_exact(&mut skip)?;
    }

    let mut written: u64 = 0;
    let mut consumed: u64 = file_hdr_sz as u64;
    let zeros = vec![0u8; blk_sz as usize];

    for _ in 0..total_chunks {
        let ctype = rd_u16(&mut r)?;
        let _res = rd_u16(&mut r)?;
        let chunk_blocks = rd_u32(&mut r)? as u64;
        let total_sz = rd_u32(&mut r)? as u64;
        if chunk_hdr_sz as usize > 12 {
            let mut skip = vec![0u8; chunk_hdr_sz as usize - 12];
            r.read_exact(&mut skip)?;
        }
        consumed += chunk_hdr_sz as u64;
        let out_bytes = chunk_blocks * blk_sz;

        match ctype {
            CHUNK_RAW => {
                // Derive the data length from the block count, not `total_sz`: it's authoritative
                // and stays correct even for images whose `total_sz` overflowed u32 on a >4 GiB RAW
                // run (flashers do the same). `total_sz` is only a (possibly wrong) byte hint.
                let data_len = chunk_blocks * blk_sz;
                let _ = total_sz;
                let mut remaining = data_len;
                let mut buf = vec![0u8; 1 << 20];
                while remaining > 0 {
                    let n = remaining.min(buf.len() as u64) as usize;
                    r.read_exact(&mut buf[..n])?;
                    w.write_all(&buf[..n])?;
                    remaining -= n as u64;
                    written += n as u64;
                }
                consumed += data_len;
            }
            CHUNK_FILL => {
                let fill = rd_u32(&mut r)?;
                consumed += 4;
                let word = fill.to_le_bytes();
                let mut block = Vec::with_capacity(blk_sz as usize);
                while (block.len() as u64) < blk_sz {
                    block.extend_from_slice(&word);
                }
                block.truncate(blk_sz as usize);
                for _ in 0..chunk_blocks {
                    w.write_all(&block)?;
                    written += blk_sz;
                }
            }
            CHUNK_DONT_CARE => {
                for _ in 0..chunk_blocks {
                    w.write_all(&zeros)?;
                    written += blk_sz;
                }
            }
            CHUNK_CRC32 => {
                let _ = rd_u32(&mut r)?;
                consumed += 4;
            }
            other => bail!("unknown sparse chunk type {other:#06x}"),
        }
        let _ = out_bytes;
        if let Some(cb) = progress.as_deref_mut() {
            cb(consumed.min(total_in), total_in);
        }
    }
    w.flush()?;
    Ok(written)
}

/// Encode a flat raw image into Android sparse format.
pub fn raw_to_sparse(
    input: &Path,
    output: &Path,
    block_size: u32,
    mut progress: Option<Progress>,
) -> Result<()> {
    let fin = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let total = fin.metadata()?.len();
    let blk = block_size as u64;
    if total % blk != 0 {
        // libsparse requires whole blocks; pad the tail with zeros.
    }
    let total_blocks = total.div_ceil(blk);
    if total_blocks > u32::MAX as u64 {
        bail!("image too large for sparse v1 ({total_blocks} blocks)");
    }

    let mut r = BufReader::with_capacity(1 << 20, fin);
    let out = File::create(output)?;
    let mut w = BufWriter::with_capacity(1 << 20, out);

    // Reserve the file header; patch total_chunks at the end.
    write_file_header(&mut w, block_size, total_blocks as u32, 0)?;

    let mut chunks: u32 = 0;
    let mut read_block = vec![0u8; block_size as usize];

    // State for a pending RAW run we're accumulating into the output.
    let mut raw_run_hdr_pos: Option<u64> = None;
    let mut raw_run_blocks: u32 = 0;

    let mut done_blocks: u64 = 0;
    while done_blocks < total_blocks {
        // Read one block (zero-padded if the final block is short).
        let want = block_size as usize;
        let mut filled = 0usize;
        while filled < want {
            match r.read(&mut read_block[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        for b in &mut read_block[filled..] {
            *b = 0;
        }

        match fill_word(&read_block) {
            Some(word) => {
                // Close any open RAW run first.
                if let Some(pos) = raw_run_hdr_pos.take() {
                    patch_raw_header(&mut w, pos, raw_run_blocks, block_size)?;
                    raw_run_blocks = 0;
                }
                write_fill_chunk(&mut w, word, 1)?;
                chunks += 1;
            }
            None => {
                if raw_run_hdr_pos.is_none() {
                    // Start a RAW run: placeholder header, patched when it ends.
                    let pos = w.stream_position()?;
                    write_chunk_header(&mut w, CHUNK_RAW, 0, 0)?;
                    raw_run_hdr_pos = Some(pos);
                    raw_run_blocks = 0;
                    chunks += 1;
                }
                w.write_all(&read_block)?;
                raw_run_blocks += 1;
                // Cap the run so its `total_sz` (u32) can never overflow; start a fresh RAW chunk
                // on the next non-fill block.
                if raw_run_blocks >= MAX_RAW_RUN_BLOCKS {
                    let pos = raw_run_hdr_pos.take().unwrap();
                    patch_raw_header(&mut w, pos, raw_run_blocks, block_size)?;
                    raw_run_blocks = 0;
                }
            }
        }

        done_blocks += 1;
        if done_blocks % 4096 == 0 {
            if let Some(cb) = progress.as_deref_mut() {
                cb(done_blocks * blk, total);
            }
        }
    }
    if let Some(pos) = raw_run_hdr_pos.take() {
        patch_raw_header(&mut w, pos, raw_run_blocks, block_size)?;
    }

    // Patch total_chunks in the file header.
    w.flush()?;
    let mut f = w.into_inner()?;
    f.seek(SeekFrom::Start(24))?;
    f.write_all(&chunks.to_le_bytes())?;
    f.flush()?;
    if let Some(cb) = progress.as_deref_mut() {
        cb(total, total);
    }
    Ok(())
}

/// Return `Some(word)` if every 32-bit lane of the block equals the same word.
fn fill_word(block: &[u8]) -> Option<u32> {
    if block.len() % 4 != 0 {
        return None;
    }
    let first = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
    for chunk in block.chunks_exact(4) {
        let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if w != first {
            return None;
        }
    }
    Some(first)
}

fn write_file_header(
    w: &mut impl Write,
    blk_sz: u32,
    total_blks: u32,
    total_chunks: u32,
) -> Result<()> {
    w.write_all(&SPARSE_MAGIC.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // major
    w.write_all(&0u16.to_le_bytes())?; // minor
    w.write_all(&FILE_HDR_SZ.to_le_bytes())?;
    w.write_all(&CHUNK_HDR_SZ.to_le_bytes())?;
    w.write_all(&blk_sz.to_le_bytes())?;
    w.write_all(&total_blks.to_le_bytes())?;
    w.write_all(&total_chunks.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // image checksum (unused by flashers)
    Ok(())
}

fn write_chunk_header(
    w: &mut impl Write,
    ctype: u16,
    chunk_blocks: u32,
    total_sz: u32,
) -> Result<()> {
    w.write_all(&ctype.to_le_bytes())?;
    w.write_all(&0u16.to_le_bytes())?; // reserved
    w.write_all(&chunk_blocks.to_le_bytes())?;
    w.write_all(&total_sz.to_le_bytes())?;
    Ok(())
}

fn write_fill_chunk(w: &mut impl Write, word: u32, blocks: u32) -> Result<()> {
    write_chunk_header(w, CHUNK_FILL, blocks, CHUNK_HDR_SZ as u32 + 4)?;
    w.write_all(&word.to_le_bytes())?;
    Ok(())
}

/// Seek back to a RAW chunk's placeholder header and write the real counts.
fn patch_raw_header(
    w: &mut BufWriter<File>,
    hdr_pos: u64,
    blocks: u32,
    block_size: u32,
) -> Result<()> {
    let total_sz_u64 = CHUNK_HDR_SZ as u64 + blocks as u64 * block_size as u64;
    if total_sz_u64 > u32::MAX as u64 {
        bail!("RAW chunk too large for sparse v1 ({total_sz_u64} bytes)");
    }
    let total_sz = total_sz_u64 as u32;
    let cur = w.stream_position()?;
    w.flush()?;
    let f = w.get_mut();
    f.seek(SeekFrom::Start(hdr_pos))?;
    f.write_all(&CHUNK_RAW.to_le_bytes())?;
    f.write_all(&0u16.to_le_bytes())?;
    f.write_all(&blocks.to_le_bytes())?;
    f.write_all(&total_sz.to_le_bytes())?;
    f.seek(SeekFrom::Start(cur))?;
    Ok(())
}
