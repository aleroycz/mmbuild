// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! ANDROID! boot/recovery image parse and rebuild (header v0-2).

use anyhow::{bail, Result};

const MAGIC: &[u8; 8] = b"ANDROID!";
/// MagiskBoot `SEANDROID_MAGIC` - Samsung bootloader looks for this after payload.
const SEANDROID_MAGIC: &[u8] = b"SEANDROIDENFORCE";
/// MagiskBoot `SIGNER_VER02_MAGIC` / `SIGNER_VER02_SZ`.
const SIGNER_VER02_MAGIC: &[u8] = b"SignerVer02";
const SIGNER_VER02_SZ: usize = 512;

#[derive(Debug, Clone)]
pub struct BootImage {
    pub header_version: u32,
    pub page_size: u32,
    pub kernel_addr: u32,
    pub ramdisk_addr: u32,
    pub second_addr: u32,
    pub tags_addr: u32,
    pub os_version: u32,
    pub name: String,
    pub cmdline: String,
    pub extra_cmdline: String,
    pub recovery_dtbo_offset: u64,
    pub header_size: u32,
    pub dtb_addr: u64,
    pub kernel: Vec<u8>,
    pub ramdisk: Vec<u8>,
    pub second: Vec<u8>,
    pub recovery_dtbo: Vec<u8>,
    pub dtb: Vec<u8>,
    /// MagiskBoot proprietary tail: `SEANDROIDENFORCE` (+ optional 512-byte
    /// `SignerVer02` blob copied from stock). Required for Samsung boot on
    /// A125F even when vbmeta verification is disabled - Magisk always
    /// re-appends this on repack; zero-padding alone is not enough.
    pub samsung_tail: Vec<u8>,
}

fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}
fn npages(size: u32, page: u32) -> u64 {
    if page == 0 {
        return 0;
    }
    (size as u64).div_ceil(page as u64)
}
fn npages_usize(size: usize, page: usize) -> usize {
    if page == 0 {
        return 0;
    }
    size.div_ceil(page)
}

impl BootImage {
    pub fn parse(buf: &[u8]) -> Result<BootImage> {
        if buf.len() < 1660 || &buf[0..8] != MAGIC {
            bail!("not an Android boot image (missing ANDROID! magic)");
        }
        let kernel_size = rd_u32(buf, 8);
        let ramdisk_size = rd_u32(buf, 16);
        let second_size = rd_u32(buf, 24);
        let page_size = rd_u32(buf, 36);
        let header_version = rd_u32(buf, 40);
        if page_size == 0 {
            bail!("boot image has zero page_size");
        }

        let (mut dtbo_size, mut dtb_size) = (0u32, 0u32);
        let mut recovery_dtbo_offset = 0u64;
        let mut header_size = 0u32;
        let mut dtb_addr = 0u64;
        if header_version >= 1 {
            dtbo_size = rd_u32(buf, 1632);
            recovery_dtbo_offset = rd_u64(buf, 1636);
            header_size = rd_u32(buf, 1644);
        }
        if header_version >= 2 {
            dtb_size = rd_u32(buf, 1648);
            dtb_addr = rd_u64(buf, 1652);
        }

        // Section offsets, each page-aligned, header occupies page 0..n.
        let p = page_size as u64;
        let mut off = npages(1, page_size) * p; // 1 page for header region
                                                // (header region is exactly 1 page since header_size <= page_size)
        off = p.max(off);
        let kernel = slice(buf, off, kernel_size)?;
        off += npages(kernel_size, page_size) * p;
        let ramdisk = slice(buf, off, ramdisk_size)?;
        off += npages(ramdisk_size, page_size) * p;
        let second = slice(buf, off, second_size)?;
        off += npages(second_size, page_size) * p;
        let recovery_dtbo = if dtbo_size > 0 {
            let d = slice(buf, off, dtbo_size)?;
            off += npages(dtbo_size, page_size) * p;
            d
        } else {
            Vec::new()
        };
        let dtb = if dtb_size > 0 {
            let d = slice(buf, off, dtb_size)?;
            off += npages(dtb_size, page_size) * p;
            d
        } else {
            Vec::new()
        };

        // MagiskBoot: capture SEANDROIDENFORCE (+ SignerVer02) after AOSP payload.
        let samsung_tail = extract_samsung_tail(buf, off as usize);

        Ok(BootImage {
            header_version,
            page_size,
            kernel_addr: rd_u32(buf, 12),
            ramdisk_addr: rd_u32(buf, 20),
            second_addr: rd_u32(buf, 28),
            tags_addr: rd_u32(buf, 32),
            os_version: rd_u32(buf, 44),
            name: cstr(&buf[48..64]),
            cmdline: cstr(&buf[64..576]),
            extra_cmdline: cstr(&buf[608..1632]),
            recovery_dtbo_offset,
            header_size,
            dtb_addr,
            kernel,
            ramdisk,
            second,
            recovery_dtbo,
            dtb,
            samsung_tail,
        })
    }

    /// Rebuild boot image; recompute sizes, dtbo offset, and SHA-1 id.
    ///
    /// If `pad_to` is `Some(n)` and the image is shorter than `n`, zero-pad to
    /// `n` (Samsung AP images are partition-sized, typically 32 MiB, with an
    /// AVB footer in the trailing padding - we drop the stock footer and rely
    /// on a patched vbmeta with verification disabled).
    pub fn build(&self) -> Vec<u8> {
        self.build_padded(None)
    }

    pub fn build_padded(&self, pad_to: Option<usize>) -> Vec<u8> {
        let page = self.page_size as usize;
        /* Layout offsets (header occupies page 0). */
        let mut off = page;
        off += npages_usize(self.kernel.len(), page) * page;
        off += npages_usize(self.ramdisk.len(), page) * page;
        off += npages_usize(self.second.len(), page) * page;
        let dtbo_offset = if self.header_version >= 1 && !self.recovery_dtbo.is_empty() {
            off as u64
        } else {
            0
        };

        let mut hdr = vec![0u8; page];
        hdr[0..8].copy_from_slice(MAGIC);
        wr_u32(&mut hdr, 8, self.kernel.len() as u32);
        wr_u32(&mut hdr, 12, self.kernel_addr);
        wr_u32(&mut hdr, 16, self.ramdisk.len() as u32);
        wr_u32(&mut hdr, 20, self.ramdisk_addr);
        wr_u32(&mut hdr, 24, self.second.len() as u32);
        wr_u32(&mut hdr, 28, self.second_addr);
        wr_u32(&mut hdr, 32, self.tags_addr);
        wr_u32(&mut hdr, 36, self.page_size);
        wr_u32(&mut hdr, 40, self.header_version);
        wr_u32(&mut hdr, 44, self.os_version);
        put_str(&mut hdr, 48, &self.name, 16);
        put_str(&mut hdr, 64, &self.cmdline, 512);
        // id at 576..608 filled in after we compute it.
        put_str(&mut hdr, 608, &self.extra_cmdline, 1024);
        if self.header_version >= 1 {
            wr_u32(&mut hdr, 1632, self.recovery_dtbo.len() as u32);
            wr_u64(&mut hdr, 1636, dtbo_offset);
            let hsz = if self.header_version >= 2 { 1660 } else { 1648 };
            wr_u32(&mut hdr, 1644, hsz);
        }
        if self.header_version >= 2 {
            wr_u32(&mut hdr, 1648, self.dtb.len() as u32);
            wr_u64(&mut hdr, 1652, self.dtb_addr);
        }

        // SHA-1 id = hash of each section's bytes followed by its u32 size.
        let id = self.compute_id();
        hdr[576..596].copy_from_slice(&id);

        let mut out = hdr;
        push_padded(&mut out, &self.kernel, page);
        push_padded(&mut out, &self.ramdisk, page);
        push_padded(&mut out, &self.second, page);
        if self.header_version >= 1 && !self.recovery_dtbo.is_empty() {
            push_padded(&mut out, &self.recovery_dtbo, page);
        }
        if self.header_version >= 2 && !self.dtb.is_empty() {
            push_padded(&mut out, &self.dtb, page);
        }
        // MagiskBoot proprietary Samsung tail (immediately after page-aligned payload).
        if !self.samsung_tail.is_empty() {
            out.extend_from_slice(&self.samsung_tail);
        }
        if let Some(n) = pad_to {
            if out.len() < n {
                out.resize(n, 0);
            }
        }
        out
    }

    fn compute_id(&self) -> [u8; 20] {
        let mut sha = Sha1::new();
        let feed = |sha: &mut Sha1, data: &[u8]| {
            sha.update(data);
            sha.update(&(data.len() as u32).to_le_bytes());
        };
        feed(&mut sha, &self.kernel);
        feed(&mut sha, &self.ramdisk);
        feed(&mut sha, &self.second);
        if self.header_version >= 1 {
            feed(&mut sha, &self.recovery_dtbo);
        }
        if self.header_version >= 2 {
            feed(&mut sha, &self.dtb);
        }
        sha.finish()
    }
}

/// MagiskBoot repack: `SEANDROIDENFORCE` + optional 512-byte `SignerVer02` blob.
fn extract_samsung_tail(buf: &[u8], payload_end: usize) -> Vec<u8> {
    if payload_end + SEANDROID_MAGIC.len() > buf.len() {
        return Vec::new();
    }
    if &buf[payload_end..payload_end + SEANDROID_MAGIC.len()] != SEANDROID_MAGIC {
        // Some images have a small gap; search forward a page.
        let search_to = (payload_end + 4096).min(buf.len());
        if let Some(rel) = buf[payload_end..search_to]
            .windows(SEANDROID_MAGIC.len())
            .position(|w| w == SEANDROID_MAGIC)
        {
            return extract_samsung_tail(buf, payload_end + rel);
        }
        return Vec::new();
    }
    let mut tail = SEANDROID_MAGIC.to_vec();
    let sig_off = payload_end + SEANDROID_MAGIC.len();
    if sig_off + SIGNER_VER02_SZ <= buf.len() && buf[sig_off..].starts_with(SIGNER_VER02_MAGIC) {
        tail.extend_from_slice(&buf[sig_off..sig_off + SIGNER_VER02_SZ]);
    }
    tail
}

fn slice(buf: &[u8], off: u64, size: u32) -> Result<Vec<u8>> {
    let off = off as usize;
    let end = off + size as usize;
    if end > buf.len() {
        bail!(
            "boot image section [{off}..{end}] runs past end ({})",
            buf.len()
        );
    }
    Ok(buf[off..end].to_vec())
}
fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn put_str(b: &mut [u8], o: usize, s: &str, cap: usize) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(cap - 1);
    b[o..o + n].copy_from_slice(&bytes[..n]);
}
fn push_padded(out: &mut Vec<u8>, data: &[u8], page: usize) {
    out.extend_from_slice(data);
    let rem = out.len() % page;
    if rem != 0 {
        out.extend(std::iter::repeat(0u8).take(page - rem));
    }
}

/// Extract raw boot/recovery image from AP tar/lz4.
pub fn is_boot_magic(buf: &[u8]) -> bool {
    buf.len() >= 8 && &buf[0..8] == MAGIC
}

// --------------------------- minimal SHA-1 -------------------------------
// Minimal RFC 3174 SHA-1 for boot image id only (not a security hash).

struct Sha1 {
    h: [u32; 5],
    buf: [u8; 64],
    buflen: usize,
    len: u64,
}
impl Sha1 {
    fn new() -> Sha1 {
        Sha1 {
            h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buf: [0; 64],
            buflen: 0,
            len: 0,
        }
    }
    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let need = 64 - self.buflen;
            let take = need.min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let block = self.buf;
                self.process(&block);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.process(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }
    fn finish(mut self) -> [u8; 20] {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buflen != 56 {
            self.update(&[0x00]);
        }
        let be = bits.to_be_bytes();
        self.update(&be);
        let mut out = [0u8; 20];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
    fn process(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (self.h[0], self.h[1], self.h[2], self.h[3], self.h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known_vectors() {
        let mut s = Sha1::new();
        s.update(b"abc");
        assert_eq!(hex(&s.finish()), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let mut s = Sha1::new();
        s.update(b"");
        assert_eq!(hex(&s.finish()), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        // Multi-block (>64 bytes) to exercise buffering.
        let mut s = Sha1::new();
        s.update(&[b'a'; 1000]);
        assert_eq!(hex(&s.finish()), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn mkimg() -> BootImage {
        BootImage {
            header_version: 2,
            page_size: 2048,
            kernel_addr: 0x8000,
            ramdisk_addr: 0x1000000,
            second_addr: 0,
            tags_addr: 0x100,
            os_version: 0,
            name: "TEST".into(),
            cmdline: "console=ttyS0".into(),
            extra_cmdline: String::new(),
            recovery_dtbo_offset: 0,
            header_size: 1660,
            dtb_addr: 0,
            kernel: vec![1, 2, 3, 4, 5],
            ramdisk: vec![9; 5000],
            second: Vec::new(),
            recovery_dtbo: Vec::new(),
            dtb: vec![7, 7, 7],
            samsung_tail: Vec::new(),
        }
    }

    #[test]
    fn roundtrip() {
        let img = mkimg();
        let bytes = img.build();
        let back = BootImage::parse(&bytes).unwrap();
        assert_eq!(back.page_size, 2048);
        assert_eq!(back.header_version, 2);
        assert_eq!(back.kernel, img.kernel);
        assert_eq!(back.ramdisk, img.ramdisk);
        assert_eq!(back.dtb, img.dtb);
        assert_eq!(back.cmdline, "console=ttyS0");
        assert_eq!(back.name, "TEST");
        // Rebuilding the parsed image is byte-identical (stable id).
        assert_eq!(back.build(), bytes);
    }

    #[test]
    fn dtbo_offset_tracks_ramdisk_size() {
        let mut img = mkimg();
        img.header_version = 1;
        img.recovery_dtbo = vec![3; 100];
        img.recovery_dtbo_offset = 999999; // stale; build must recompute
        let bytes = img.build();
        let back = BootImage::parse(&bytes).unwrap();
        let page = img.page_size as usize;
        let expect = page
            + npages_usize(img.kernel.len(), page) * page
            + npages_usize(img.ramdisk.len(), page) * page
            + npages_usize(img.second.len(), page) * page;
        assert_eq!(back.recovery_dtbo_offset, expect as u64);
        assert_eq!(back.recovery_dtbo, img.recovery_dtbo);
    }

    #[test]
    fn pad_to_preserves_payload() {
        let img = mkimg();
        let raw = img.build();
        let padded = img.build_padded(Some(raw.len() + 4096));
        assert_eq!(padded.len(), raw.len() + 4096);
        assert_eq!(&padded[..raw.len()], &raw[..]);
    }

    #[test]
    fn samsung_tail_roundtrip() {
        let mut img = mkimg();
        let mut tail = b"SEANDROIDENFORCE".to_vec();
        let mut sig = vec![0u8; SIGNER_VER02_SZ];
        sig[..11].copy_from_slice(b"SignerVer02");
        tail.extend_from_slice(&sig);
        img.samsung_tail = tail.clone();
        let bytes = img.build_padded(Some(64 * 1024));
        let back = BootImage::parse(&bytes).unwrap();
        assert_eq!(back.samsung_tail, tail);
        assert!(bytes.windows(16).any(|w| w == b"SEANDROIDENFORCE"));
    }
}
