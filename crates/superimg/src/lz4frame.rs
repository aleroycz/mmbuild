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

//! Standard lz4 frame codec for Samsung `.lz4` (1 MiB blocks, content size).

#![allow(clippy::needless_option_as_deref)] /* Option<&mut dyn FnMut> needs as_deref_mut */

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use lz4_flex::frame::{BlockSize, FrameDecoder, FrameEncoder, FrameInfo};

pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

/// Compress `input` into a standard lz4 frame at `output`.
pub fn compress_file(input: &Path, output: &Path, mut progress: Option<Progress>) -> Result<()> {
    let fin = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let total = fin.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 20, fin);
    let w = BufWriter::with_capacity(1 << 20, File::create(output)?);

    let mut info = FrameInfo::new();
    info.block_size = BlockSize::Max1MB; // lz4 -B6
    info.content_size = Some(total); // lz4 --content-size
    info.content_checksum = true; // lz4 default: trailing xxh32

    let mut enc = FrameEncoder::with_frame_info(info, w);
    let mut buf = vec![0u8; 1 << 20];
    let mut done: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        enc.write_all(&buf[..n])?;
        done += n as u64;
        if let Some(cb) = progress.as_deref_mut() {
            cb(done, total);
        }
    }
    let mut w = enc.finish()?;
    w.flush()?;
    Ok(())
}

/// Decompress a standard lz4 frame `input` into `output`.
pub fn decompress_file(input: &Path, output: &Path, mut progress: Option<Progress>) -> Result<u64> {
    let fin = File::open(input).with_context(|| format!("open {}", input.display()))?;
    let compressed = fin.metadata()?.len();
    let r = BufReader::with_capacity(1 << 20, fin);
    let mut dec = FrameDecoder::new(r);
    let mut w = BufWriter::with_capacity(1 << 20, File::create(output)?);
    let mut buf = vec![0u8; 1 << 20];
    let mut out: u64 = 0;
    loop {
        let n = dec.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        out += n as u64;
        if let Some(cb) = progress.as_deref_mut() {
            // We don't know output size up front; report compressed consumed.
            cb(out, out.max(compressed));
        }
    }
    w.flush()?;
    Ok(out)
}

/// True if the file starts with the standard lz4 frame magic.
pub fn is_lz4(path: &Path) -> Result<bool> {
    let mut f = File::open(path)?;
    let mut m = [0u8; 4];
    if f.read_exact(&mut m).is_err() {
        return Ok(false);
    }
    Ok(m == [0x04, 0x22, 0x4D, 0x18])
}

/// True if `buf` starts with the standard lz4 frame magic.
pub fn is_lz4_bytes(buf: &[u8]) -> bool {
    buf.len() >= 4 && buf[0..4] == [0x04, 0x22, 0x4D, 0x18]
}

/// Decompress a standard lz4 frame from memory.
pub fn decompress_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut dec = FrameDecoder::new(std::io::Cursor::new(input));
    let mut out = Vec::new();
    dec.read_to_end(&mut out).context("lz4 frame decompress")?;
    Ok(out)
}

/// Compress bytes into a standard lz4 frame (`-B6 --content-size`).
pub fn compress_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut info = FrameInfo::new();
    info.block_size = BlockSize::Max1MB;
    info.content_size = Some(input.len() as u64);
    info.content_checksum = true;
    let mut enc = FrameEncoder::with_frame_info(info, Vec::new());
    enc.write_all(input).context("lz4 frame compress")?;
    enc.finish().context("lz4 frame finish")
}
