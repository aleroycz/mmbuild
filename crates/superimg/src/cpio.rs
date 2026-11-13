// SPDX-License-Identifier: GPL-3.0-or-later
//
// MicroMiner OS — a custom Android init/OS and build toolchain.
// Copyright (C) 2026 Vakea
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

//! newc (070701) cpio read/write for Android initramfs.

use anyhow::{bail, Result};

const MAGIC: &[u8; 6] = b"070701";
const TRAILER: &str = "TRAILER!!!";

/// Mode bits (the type portion of st_mode).
pub const S_IFMT: u32 = 0o170000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;

/// One archive member.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// Original newc inode (0 = assign on write). MagiskBoot keeps stock inos.
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub nlink: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub rdev_major: u32,
    pub rdev_minor: u32,
    pub data: Vec<u8>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    /// A regular file with the given permission bits (e.g. 0o755).
    pub fn file(name: &str, perm: u32, data: Vec<u8>) -> Entry {
        Entry::new(name, S_IFREG | (perm & 0o7777), data)
    }
    /// A directory with the given permission bits.
    pub fn dir(name: &str, perm: u32) -> Entry {
        Entry::new(name, S_IFDIR | (perm & 0o7777), Vec::new())
    }
    /// A symlink `name` -> `target`.
    pub fn symlink(name: &str, target: &str) -> Entry {
        Entry::new(name, S_IFLNK | 0o777, target.as_bytes().to_vec())
    }

    fn new(name: &str, mode: u32, data: Vec<u8>) -> Entry {
        Entry {
            name: name.to_string(),
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            mtime: 0,
            nlink: 1,
            dev_major: 0,
            dev_minor: 0,
            rdev_major: 0,
            rdev_minor: 0,
            data,
        }
    }
}

fn pad4(n: usize) -> usize {
    (4 - (n & 3)) & 3
}

/// Parse a newc cpio archive into its entries (TRAILER dropped).
pub fn read(buf: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    loop {
        if off + 110 > buf.len() {
            break;
        }
        if &buf[off..off + 6] != MAGIC {
            // Some archives pad to a 512-byte boundary with zeros before the next concatenated
            // section; skip run of zeros.
            if buf[off] == 0 {
                off += 1;
                continue;
            }
            bail!(
                "bad cpio magic at offset {off}: {:?}",
                &buf[off..off + 6.min(buf.len() - off)]
            );
        }
        let f = |i: usize| hex8(&buf[off + 6 + i * 8..off + 6 + i * 8 + 8]);
        let ino = f(0)?;
        let mode = f(1)?;
        let uid = f(2)?;
        let gid = f(3)?;
        let nlink = f(4)?;
        let mtime = f(5)?;
        let filesize = f(6)? as usize;
        let dev_major = f(7)?;
        let dev_minor = f(8)?;
        let rdev_major = f(9)?;
        let rdev_minor = f(10)?;
        let namesize = f(11)? as usize;
        // check field (f(12)) ignored.

        let name_off = off + 110;
        if name_off + namesize > buf.len() {
            bail!("cpio name runs past end");
        }
        let raw_name = &buf[name_off..name_off + namesize];
        let name =
            String::from_utf8_lossy(raw_name.strip_suffix(&[0]).unwrap_or(raw_name)).to_string();
        // Header + name padded to 4 bytes.
        let data_off = name_off + namesize + pad4(110 + namesize);
        if name == TRAILER {
            break;
        }
        if data_off + filesize > buf.len() {
            bail!("cpio data runs past end for {name}");
        }
        let data = buf[data_off..data_off + filesize].to_vec();
        off = data_off + filesize + pad4(filesize);

        entries.push(Entry {
            name,
            ino,
            mode,
            uid,
            gid,
            mtime,
            nlink,
            dev_major,
            dev_minor,
            rdev_major,
            rdev_minor,
            data,
        });
    }
    Ok(entries)
}

/// Serialize entries into a newc cpio archive (TRAILER appended, padded to 512).
/// Preserves stock inode numbers (MagiskBoot); assigns fresh inos to new entries.
pub fn write(entries: &[Entry]) -> Vec<u8> {
    let mut next_ino = entries
        .iter()
        .map(|e| e.ino)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    if next_ino == 0 {
        next_ino = 1;
    }
    let mut out = Vec::new();
    for e in entries {
        let ino = if e.ino != 0 {
            e.ino
        } else {
            let i = next_ino;
            next_ino = next_ino.saturating_add(1);
            i
        };
        write_entry(&mut out, e, ino);
    }
    // Trailer: an entry named "TRAILER!!!" with nlink=1 and all else zero.
    let trailer = Entry {
        name: TRAILER.to_string(),
        ino: 0,
        mode: 0,
        uid: 0,
        gid: 0,
        mtime: 0,
        nlink: 1,
        dev_major: 0,
        dev_minor: 0,
        rdev_major: 0,
        rdev_minor: 0,
        data: Vec::new(),
    };
    write_entry(&mut out, &trailer, 0);
    // Pad the whole archive to a 512-byte boundary (bootimg ramdisks expect it).
    while out.len() % 512 != 0 {
        out.push(0);
    }
    out
}

fn write_entry(out: &mut Vec<u8>, e: &Entry, ino: u32) {
    let name_bytes = e.name.as_bytes();
    let namesize = name_bytes.len() + 1; // include trailing NUL
    out.extend_from_slice(MAGIC);
    let fields = [
        ino,
        e.mode,
        e.uid,
        e.gid,
        e.nlink,
        e.mtime,
        e.data.len() as u32,
        e.dev_major,
        e.dev_minor,
        e.rdev_major,
        e.rdev_minor,
        namesize as u32,
        0, // check
    ];
    for v in fields {
        out.extend_from_slice(format!("{v:08x}").as_bytes());
    }
    out.extend_from_slice(name_bytes);
    out.push(0);
    for _ in 0..pad4(110 + namesize) {
        out.push(0);
    }
    out.extend_from_slice(&e.data);
    for _ in 0..pad4(e.data.len()) {
        out.push(0);
    }
}

fn hex8(b: &[u8]) -> Result<u32> {
    let s = std::str::from_utf8(b).map_err(|_| anyhow::anyhow!("non-ascii cpio field"))?;
    u32::from_str_radix(s, 16).map_err(|_| anyhow::anyhow!("bad hex cpio field {s:?}"))
}
