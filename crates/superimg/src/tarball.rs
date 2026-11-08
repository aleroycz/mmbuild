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

//! Minimal POSIX ustar writer for Odin AP packages.
//!
//! Odin accepts a plain `.tar` of the `*.img.lz4` members (it only verifies an
//! appended MD5 when the file is named `*.tar.md5`). We emit ustar headers with
//! empty uname/gname and mode 0644 — the same shape the reference
//! `tar -H ustar` produced — so the result flashes identically but is built with
//! no external `tar`.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

pub struct TarBuilder<W: Write> {
    w: W,
}

impl<W: Write> TarBuilder<W> {
    pub fn new(w: W) -> Self {
        TarBuilder { w }
    }

    /// Append a file from disk under the archive name `name`.
    pub fn append_path(&mut self, name: &str, path: &Path) -> Result<()> {
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let size = f.metadata()?.len();
        self.write_header(name, size, b'0')?;
        let mut r = BufReader::with_capacity(1 << 20, f);
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            self.w.write_all(&buf[..n])?;
            written += n as u64;
        }
        if written != size {
            bail!("short read on {} ({written} of {size})", path.display());
        }
        self.pad_to_block(size)?;
        Ok(())
    }

    /// Append in-memory bytes under the archive name `name`.
    pub fn append_bytes(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.write_header(name, data.len() as u64, b'0')?;
        self.w.write_all(data)?;
        self.pad_to_block(data.len() as u64)?;
        Ok(())
    }

    /// Write the two trailing zero blocks that terminate the archive.
    pub fn finish(mut self) -> Result<W> {
        let zero = [0u8; 512];
        self.w.write_all(&zero)?;
        self.w.write_all(&zero)?;
        self.w.flush()?;
        Ok(self.w)
    }

    fn write_header(&mut self, name: &str, size: u64, typeflag: u8) -> Result<()> {
        if name.len() > 100 {
            bail!("tar name too long for ustar name field: {name}");
        }
        let mut h = [0u8; 512];
        put(&mut h, 0, 100, name.as_bytes());
        put_octal(&mut h, 100, 8, 0o644); // mode
        put_octal(&mut h, 108, 8, 0); // uid
        put_octal(&mut h, 116, 8, 0); // gid
        put_octal_size(&mut h, 124, 12, size); // size
        put_octal_size(&mut h, 136, 12, 0); // mtime (0 = reproducible)
        // checksum field (148..156) left as spaces for the sum
        for b in &mut h[148..156] {
            *b = b' ';
        }
        h[156] = typeflag;
        put(&mut h, 257, 6, b"ustar\0");
        put(&mut h, 263, 2, b"00");
        // uname/gname intentionally empty.
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        // ustar checksum: 6 octal digits, NUL, space.
        let cs = format!("{:06o}\0 ", sum);
        put(&mut h, 148, 8, cs.as_bytes());
        self.w.write_all(&h)?;
        Ok(())
    }

    fn pad_to_block(&mut self, size: u64) -> Result<()> {
        let rem = (size % 512) as usize;
        if rem != 0 {
            let pad = [0u8; 512];
            self.w.write_all(&pad[..512 - rem])?;
        }
        Ok(())
    }
}

fn put(buf: &mut [u8], off: usize, len: usize, src: &[u8]) {
    let n = src.len().min(len);
    buf[off..off + n].copy_from_slice(&src[..n]);
}

fn put_octal(buf: &mut [u8], off: usize, len: usize, val: u64) {
    // width = len-1 octal digits, then NUL.
    let s = format!("{:0width$o}\0", val, width = len - 1);
    put(buf, off, len, s.as_bytes());
}

fn put_octal_size(buf: &mut [u8], off: usize, len: usize, val: u64) {
    let s = format!("{:0width$o}\0", val, width = len - 1);
    put(buf, off, len, s.as_bytes());
}
