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

//! ADB RSA token sign and RSAPublicKey blob for adbd.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;

const MODULUS_BYTES: usize = 256; // 2048-bit
const MODULUS_WORDS: u32 = 64; // 256 / 4

/// A loaded (or freshly generated) ADB signing key.
pub struct AdbKey {
    private: RsaPrivateKey,
    public: RsaPublicKey,
}

impl AdbKey {
    /// Load `~/.android/adbkey`, or generate + save one if absent.
    pub fn load_or_create() -> Result<AdbKey> {
        let path = default_key_path()?;
        if path.exists() {
            Self::from_file(&path)
        } else {
            let key = Self::generate()?;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let pem = key
                .private
                .to_pkcs8_pem(LineEnding::LF)
                .context("encode adbkey")?;
            std::fs::write(&path, pem.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            Ok(key)
        }
    }

    /// Load a key from a PEM file, trying PKCS#8 then PKCS#1 armor.
    pub fn from_file(path: &Path) -> Result<AdbKey> {
        let pem =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let private = RsaPrivateKey::from_pkcs8_pem(&pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&pem))
            .with_context(|| format!("parse private key {}", path.display()))?;
        let public = RsaPublicKey::from(&private);
        Ok(AdbKey { private, public })
    }

    pub fn generate() -> Result<AdbKey> {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, 2048).context("generate RSA key")?;
        let public = RsaPublicKey::from(&private);
        Ok(AdbKey { private, public })
    }

    #[cfg(test)]
    pub fn from_pkcs1_der(der: &[u8]) -> Result<AdbKey> {
        let private = RsaPrivateKey::from_pkcs1_der(der)?;
        let public = RsaPublicKey::from(&private);
        Ok(AdbKey { private, public })
    }

    /// Sign the 20-byte adbd token (PKCS#1 v1.5 / SHA-1 DigestInfo).
    pub fn sign_token(&self, token: &[u8]) -> Result<Vec<u8>> {
        let sig = self
            .private
            .sign(Pkcs1v15Sign::new::<Sha1>(), token)
            .context("sign adb token")?;
        Ok(sig)
    }

    /// The Android `RSAPublicKey` blob: base64(524-byte struct) + " user\0".
    pub fn android_pubkey_line(&self, user: &str) -> Result<Vec<u8>> {
        let blob = encode_android_pubkey(&self.public)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
        let mut out = format!("{b64} {user}").into_bytes();
        out.push(0); // NUL-terminated
        Ok(out)
    }
}

fn default_key_path() -> Result<PathBuf> {
    let home = home_dir().context("cannot determine home directory for adbkey")?;
    Ok(home.join(".android").join("adbkey"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Android RSAPublicKey blob (524 bytes) for adbd AUTH.
fn encode_android_pubkey(key: &RsaPublicKey) -> Result<Vec<u8>> {
    let n = key.n();
    let modulus_le = to_fixed_le(n, MODULUS_BYTES)?;

    // n0inv = -1 / n[0..4] (mod 2^32).
    let n0 = u32::from_le_bytes([modulus_le[0], modulus_le[1], modulus_le[2], modulus_le[3]]);
    let n0inv = mont_n0inv(n0);

    // rr = R^2 mod n, with R = 2^(32 * words) = 2^2048, so rr = 2^4096 mod n.
    let r2_exp = BigUint::from(32u32 * MODULUS_WORDS * 2); // 4096
    let rr = BigUint::from(2u32).modpow(&r2_exp, n);
    let rr_le = to_fixed_le(&rr, MODULUS_BYTES)?;

    // Public exponent as u32 (standard values 3 / 65537 fit easily).
    let e_be = key.e().to_bytes_be();
    let mut exponent: u32 = 0;
    for b in e_be.iter().rev().take(4).rev() {
        exponent = (exponent << 8) | u32::from(*b);
    }
    if exponent == 0 {
        exponent = 65537;
    }

    let mut out = Vec::with_capacity(4 + 4 + MODULUS_BYTES * 2 + 4);
    out.extend_from_slice(&MODULUS_WORDS.to_le_bytes());
    out.extend_from_slice(&n0inv.to_le_bytes());
    out.extend_from_slice(&modulus_le);
    out.extend_from_slice(&rr_le);
    out.extend_from_slice(&exponent.to_le_bytes());
    debug_assert_eq!(out.len(), 524);
    Ok(out)
}

/// Big-endian BigUint -> fixed-length little-endian byte array.
fn to_fixed_le(v: &BigUint, len: usize) -> Result<Vec<u8>> {
    let be = v.to_bytes_be();
    if be.len() > len {
        anyhow::bail!("modulus larger than {len} bytes ({} bits)", v.bits());
    }
    let mut le = vec![0u8; len];
    for (i, b) in be.iter().rev().enumerate() {
        le[i] = *b;
    }
    Ok(le)
}

/// Montgomery n0inv: -(n0^-1) mod 2^32.
fn mont_n0inv(n0: u32) -> u32 {
    // Newton-Raphson: 5 iters from an odd seed cover 32 bits.
    let mut inv: u32 = 1;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(n0.wrapping_mul(inv)));
    }
    0u32.wrapping_sub(inv) // -inv (mod 2^32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n0inv_is_negative_inverse() {
        for &n0 in &[3u32, 5, 0x8000_0001, 0xFFFF_FFFF, 1_234_567_891] {
            let n0inv = mont_n0inv(n0);
            let inv = 0u32.wrapping_sub(n0inv); // = +inverse
            assert_eq!(n0.wrapping_mul(inv), 1, "inverse wrong for {n0:#x}");
        }
    }

    #[test]
    fn pubkey_blob_shape_and_roundtrip() {
        let key = AdbKey::generate().unwrap();
        let blob = encode_android_pubkey(&key.public).unwrap();
        assert_eq!(blob.len(), 524);

        // words field = 64, exponent = 65537.
        let words = u32::from_le_bytes([blob[0], blob[1], blob[2], blob[3]]);
        assert_eq!(words, 64);
        let exp = u32::from_le_bytes([blob[520], blob[521], blob[522], blob[523]]);
        assert_eq!(exp, 65537);

        // n0inv * n0 == -1 mod 2^32  (i.e. n0 * (-n0inv) == 1).
        let n0 = u32::from_le_bytes([blob[8], blob[9], blob[10], blob[11]]);
        let n0inv = u32::from_le_bytes([blob[4], blob[5], blob[6], blob[7]]);
        assert_eq!(n0.wrapping_mul(0u32.wrapping_sub(n0inv)), 1);

        // the key line ends with a NUL and contains the user tag.
        let line = key.android_pubkey_line("mmbuild@microminer").unwrap();
        assert_eq!(*line.last().unwrap(), 0u8);
        assert!(line.windows(18).any(|w| w == b"mmbuild@microminer"));
    }

    #[test]
    fn sign_token_is_rsa_sized() {
        let key = AdbKey::generate().unwrap();
        let token = [0x42u8; 20];
        let sig = key.sign_token(&token).unwrap();
        assert_eq!(sig.len(), 256); // 2048-bit signature
                                    // Verify it validates against our own public key.
        key.public
            .verify(Pkcs1v15Sign::new::<Sha1>(), &token, &sig)
            .expect("self-verify");
    }
}
