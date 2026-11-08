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

//! Samsung FUS crypto.
//!
//! Faithful reimplementation of the samloader/samfirm scheme (GPL-3.0+,
//! algorithm (C) 2020 nlscc — reimplemented here in Rust):
//!   * server nonce  : AES-256-CBC, key = KEY_1, IV = KEY_1[..16], PKCS#7
//!   * auth signature: AES-256-CBC, key = derive_key(nonce), IV = key[..16]
//!   * firmware body : AES-128-ECB, key = MD5(logic_check), PKCS#7 on last block
//!   * V4 key        : MD5(logic_check(fw_version, logic_value_factory))
//!   * V2 key (old)  : MD5("region:model:version")

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use md5::{Digest, Md5};

const KEY_1: &[u8; 32] = b"hqzdurufm2c8mf6bsjezu1qgveouv7c7";
const KEY_2: &[u8; 16] = b"w13r4cvf4hctaujv";

// AES-256-CBC
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

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

/// Decrypt the base64 `NONCE` header into the plaintext nonce string.
pub fn decrypt_nonce(enc_b64: &str) -> Result<String> {
    let mut buf = B64.decode(enc_b64.trim()).context("base64 nonce")?;
    let iv = &KEY_1[..16];
    let pt = Aes256CbcDec::new(KEY_1.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| anyhow::anyhow!("nonce decrypt: {e}"))?;
    Ok(String::from_utf8_lossy(pt).to_string())
}

fn derive_key(nonce: &str) -> Result<[u8; 32]> {
    if nonce.len() < 16 {
        bail!("nonce too short for key derivation");
    }
    let nb = nonce.as_bytes();
    let mut key = [0u8; 32];
    for i in 0..16 {
        key[i] = KEY_1[(nb[i] as usize) % 16];
    }
    key[16..].copy_from_slice(KEY_2);
    Ok(key)
}

/// Build the base64 `signature` for the Authorization header from the nonce.
pub fn get_auth(nonce: &str) -> Result<String> {
    let key = derive_key(nonce)?;
    let iv: [u8; 16] = key[..16].try_into().unwrap();
    let data = nonce.as_bytes();
    // PKCS#7 always appends between 1 and a full block of padding.
    let padded_len = (data.len() / 16 + 1) * 16;
    let mut buf = vec![0u8; padded_len];
    buf[..data.len()].copy_from_slice(data);
    let ct = Aes256CbcEnc::new(&key.into(), &iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .map_err(|e| anyhow::anyhow!("auth encrypt: {e}"))?;
    Ok(B64.encode(ct))
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

    let mut buf = vec![0u8; 1 << 20]; // multiple of 16
    let mut done: u64 = 0;
    let mut carry: Vec<u8> = Vec::new(); // hold last decrypted block to unpad at EOF
    loop {
        let n = read_full(&mut r, &mut buf)?;
        if n == 0 {
            break;
        }
        // Decrypt each 16-byte block in place.
        for block in buf[..n].chunks_exact_mut(16) {
            let mut b = GenericArray::clone_from_slice(block);
            cipher.decrypt_block(&mut b);
            block.copy_from_slice(&b);
        }
        // Flush the previous carry (now known not to be the last block).
        if !carry.is_empty() {
            w.write_all(&carry)?;
            carry.clear();
        }
        // Keep the final 16 bytes of this read as the potential last block.
        let keep = &buf[n - 16..n];
        carry.extend_from_slice(keep);
        w.write_all(&buf[..n - 16])?;
        done += n as u64;
        if let Some(cb) = progress.as_deref_mut() {
            cb(done, total);
        }
    }
    // Unpad the final block (PKCS#7).
    if !carry.is_empty() {
        let pad = *carry.last().unwrap() as usize;
        let keep = if pad >= 1 && pad <= 16 && pad <= carry.len() {
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
