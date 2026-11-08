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

//! ADB transport-layer message framing (the `amessage` struct + payload).

use anyhow::{bail, Result};

// Command constants (little-endian spelling of the 4-char code).
pub const A_CNXN: u32 = 0x4e58_4e43; // "CNXN"
pub const A_AUTH: u32 = 0x4854_5541; // "AUTH"
pub const A_OPEN: u32 = 0x4e45_504f; // "OPEN"
pub const A_OKAY: u32 = 0x5941_4b4f; // "OKAY"
pub const A_CLSE: u32 = 0x4553_4c43; // "CLSE"
pub const A_WRTE: u32 = 0x4554_5257; // "WRTE"

// A_AUTH sub-types.
pub const AUTH_TOKEN: u32 = 1;
pub const AUTH_SIGNATURE: u32 = 2;
pub const AUTH_RSAPUBLICKEY: u32 = 3;

// Connection parameters.
pub const A_VERSION: u32 = 0x0100_0001;
pub const MAX_PAYLOAD: u32 = 256 * 1024;

pub const HEADER_LEN: usize = 24;

/// A decoded/encodable ADB message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub command: u32,
    pub arg0: u32,
    pub arg1: u32,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn new(command: u32, arg0: u32, arg1: u32, payload: Vec<u8>) -> Message {
        Message { command, arg0, arg1, payload }
    }

    /// The 24-byte header for this message.
    pub fn header(&self) -> [u8; HEADER_LEN] {
        let len = self.payload.len() as u32;
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&self.command.to_le_bytes());
        h[4..8].copy_from_slice(&self.arg0.to_le_bytes());
        h[8..12].copy_from_slice(&self.arg1.to_le_bytes());
        h[12..16].copy_from_slice(&len.to_le_bytes());
        h[16..20].copy_from_slice(&checksum(&self.payload).to_le_bytes());
        h[20..24].copy_from_slice(&(self.command ^ 0xffff_ffff).to_le_bytes());
        h
    }

    /// Header followed by payload (one contiguous buffer). The transport sends
    /// header and payload as two writes, so this is used mainly by tests and as
    /// a convenience for callers that want the whole frame.
    #[allow(dead_code)]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&self.header());
        out.extend_from_slice(&self.payload);
        out
    }

    /// The command as its 4-char ASCII code (for logs), e.g. "CNXN".
    pub fn code(&self) -> String {
        self.command
            .to_le_bytes()
            .iter()
            .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
            .collect()
    }
}

/// adbd's payload checksum: the sum of all payload bytes (mod 2^32).
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, &b| acc.wrapping_add(u32::from(b)))
}

/// Parsed header fields.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub command: u32,
    pub arg0: u32,
    pub arg1: u32,
    pub data_len: u32,
    pub data_check: u32,
}

impl Header {
    /// The command as its 4-char ASCII code (for logs), e.g. "WRTE".
    pub fn command_code(&self) -> String {
        Message::new(self.command, 0, 0, Vec::new()).code()
    }
}

/// Decode a 24-byte header and validate its magic.
pub fn decode_header(buf: &[u8]) -> Result<Header> {
    if buf.len() < HEADER_LEN {
        bail!("short ADB header: {} bytes", buf.len());
    }
    let rd = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let command = rd(0);
    let magic = rd(20);
    if magic != command ^ 0xffff_ffff {
        bail!("bad ADB header magic (command {command:#010x}, magic {magic:#010x})");
    }
    Ok(Header {
        command,
        arg0: rd(4),
        arg1: rd(8),
        data_len: rd(12),
        data_check: rd(16),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_codes_spell_right() {
        assert_eq!(Message::new(A_CNXN, 0, 0, vec![]).code(), "CNXN");
        assert_eq!(Message::new(A_AUTH, 0, 0, vec![]).code(), "AUTH");
        assert_eq!(Message::new(A_WRTE, 0, 0, vec![]).code(), "WRTE");
    }

    #[test]
    fn checksum_sums_bytes() {
        assert_eq!(checksum(&[]), 0);
        assert_eq!(checksum(&[1, 2, 3]), 6);
        assert_eq!(checksum(&[0xff, 0xff]), 0x1fe);
    }

    #[test]
    fn header_roundtrips() {
        let m = Message::new(A_OPEN, 7, 0, b"shell:ls\0".to_vec());
        let enc = m.encode();
        assert_eq!(&enc[HEADER_LEN..], m.payload.as_slice());
        let h = decode_header(&enc[..HEADER_LEN]).unwrap();
        assert_eq!(h.command, A_OPEN);
        assert_eq!(h.arg0, 7);
        assert_eq!(h.data_len as usize, m.payload.len());
        assert_eq!(h.data_check, checksum(&m.payload));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut enc = Message::new(A_OKAY, 1, 2, vec![]).encode();
        enc[20] ^= 0xaa; // corrupt magic
        assert!(decode_header(&enc[..HEADER_LEN]).is_err());
    }
}
