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

//! Samsung FUS crypto (nonce auth + firmware AES keys).
//!
//! Post-2025 Smart Download auth (samloader-fus / SamFirm_Reborn >=0.3.7):
//! - Session nonce for LOGIC_CHECK = raw `NONCE` response header (opaque).
//! - Authorization signature = lowercase hex of AES-128-ECB(`AUTH_AES_KEY`,
//!   first 16 bytes of that header, padded with ASCII `'0'`).

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};

/// Auth key provided by @henr1kas / @ducthoe (SamFirm / samloader-fus).
const AUTH_AES_KEY: [u8; 16] = [
    0x42, 0x2e, 0x73, 0x73, 0x36, 0x17, 0xae, 0x2b, 0x19, 0x89, 0x40, 0xfd, 0x4e, 0x32, 0xb0, 0xa5,
];

/// `logic_check`: for each nonce char, index into `inp` by its low nibble.
pub fn logic_check(inp: &str, nonce: &str) -> Result<String> {
    if inp.len() < 16 {
        bail!("logic_check input too short ({} < 16)", inp.len());
    }
    let bytes = inp.as_bytes();
    let mut out = String::with_capacity(nonce.len());
    for c in nonce.chars() {
        let idx = (c as usize) & 0xf;
        out.push(bytes[idx] as char);
    }
    Ok(out)
}

fn auth_block(header: &str) -> [u8; 16] {
    let mut block = [b'0'; 16];
    let nb = header.as_bytes();
    let n = nb.len().min(16);
    block[..n].copy_from_slice(&nb[..n]);
    block
}

/// Keep the raw `NONCE` header as the session nonce (used in LOGIC_CHECK).
pub fn session_nonce(enc_header: &str) -> Result<String> {
    let n = enc_header.trim();
    if n.is_empty() {
        bail!("empty NONCE header");
    }
    Ok(n.to_string())
}

/// Authorization `signature` = hex(AES-128-ECB(AUTH_AES_KEY, header[:16])).
pub fn get_auth(enc_header: &str) -> Result<String> {
    let cipher = aes::Aes128::new_from_slice(&AUTH_AES_KEY).context("AES key")?;
    let mut block = GenericArray::clone_from_slice(&auth_block(enc_header.trim()));
    cipher.encrypt_block(&mut block);
    Ok(block.iter().map(|b| format!("{b:02x}")).collect())
}

fn md5_16(s: &str) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&d);
    out
}

/// V4 firmware AES key = MD5(logic_check(fw_version, logic_value_factory)).
pub fn v4_key(fw_version: &str, logic_value: &str) -> Result<[u8; 16]> {
    Ok(md5_16(&logic_check(fw_version, logic_value)?))
}

/// V2 (legacy) firmware AES key = MD5("region:model:version").
pub fn v2_key(region: &str, model: &str, version: &str) -> [u8; 16] {
    md5_16(&format!("{region}:{model}:{version}"))
}

/// Decrypt a `.enc4`/`.enc2` firmware file (AES-128-ECB) to `out`.
pub fn decrypt_file(
    input: &std::path::Path,
    output: &std::path::Path,
    key: &[u8; 16],
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<()> {
    use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
    use std::io::{BufReader, BufWriter, Write};

    let fin = std::fs::File::open(input)?;
    let total = fin.metadata()?.len();
    if total % 16 != 0 {
        bail!("encrypted file size {total} is not a multiple of 16");
    }
    let mut r = BufReader::with_capacity(1 << 20, fin);
    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(output)?);
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));

    let mut buf = vec![0u8; 1 << 20];
    let mut done: u64 = 0;
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = read_full(&mut r, &mut buf)?;
        if n == 0 {
            break;
        }
        for block in buf[..n].chunks_exact_mut(16) {
            let mut b = GenericArray::clone_from_slice(block);
            cipher.decrypt_block(&mut b);
            block.copy_from_slice(&b);
        }
        if !carry.is_empty() {
            w.write_all(&carry)?;
            carry.clear();
        }
        let keep = &buf[n - 16..n];
        carry.extend_from_slice(keep);
        w.write_all(&buf[..n - 16])?;
        done += n as u64;
        if let Some(cb) = progress.as_deref_mut() {
            cb(done, total);
        }
    }
    if !carry.is_empty() {
        let pad = *carry.last().unwrap() as usize;
        let keep = if (1..=16).contains(&pad) && pad <= carry.len() {
            carry.len() - pad
        } else {
            carry.len()
        };
        w.write_all(&carry[..keep])?;
    }
    w.flush()?;
    Ok(())
}

fn read_full(r: &mut impl std::io::Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_auth_is_hex32() {
        let sig = get_auth("abcdefghijklmnop").unwrap();
        assert_eq!(sig.len(), 32);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_nonce_passthrough() {
        let h = "iBsj/nWszkakFLDD4+lzKzIYTjeOSoQcokfwyKl+dKk=";
        assert_eq!(session_nonce(h).unwrap(), h);
    }

    #[test]
    fn logic_check_indexes_low_nibble() {
        let inp = "0123456789abcdef";
        assert_eq!(logic_check(inp, "00").unwrap(), "00");
        assert_eq!(logic_check(inp, "a").unwrap(), "1");
    }
}
