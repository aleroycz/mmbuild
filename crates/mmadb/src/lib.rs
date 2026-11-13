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

//! USB ADB client (nusb + RSA): connect, shell, file read via base64.

mod auth;
mod proto;
mod transport;

use anyhow::{bail, Context, Result};
use base64::Engine;

use auth::AdbKey;
use proto::*;
use transport::{Candidate, UsbTransport};

/// Identity reported by adbd in its CNXN banner.
#[derive(Debug, Clone, Default)]
pub struct Banner {
    pub product: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub raw: String,
}

/// A connected, authenticated ADB device.
pub struct AdbDevice {
    io: UsbTransport,
    key: AdbKey,
    maxdata: u32,
    next_local_id: u32,
    pub banner: Banner,
    pub serial: Option<String>,
}

/// True if at least one ADB interface is on the USB bus right now.
pub fn device_present() -> bool {
    UsbTransport::list().map(|c| !c.is_empty()).unwrap_or(false)
}

/// Wait for USB ADB device (None = forever).
pub fn wait_for_device(timeout: Option<std::time::Duration>) -> Result<()> {
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let mut announced = false;
    loop {
        if device_present() {
            return Ok(());
        }
        if let Some(t) = timeout {
            if start.elapsed() >= t {
                bail!("timed out after {}s waiting for an ADB device", t.as_secs());
            }
        }
        if !announced {
            eprintln!(
                "waiting for device… (enable USB debugging, connect over USB, accept the prompt)"
            );
            announced = true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

impl AdbDevice {
    /// Enumerate the bus and connect to the first authorized ADB device.
    pub fn connect_first() -> Result<AdbDevice> {
        let cands = UsbTransport::list().context("scan for ADB devices")?;
        if cands.is_empty() {
            bail!(
                "no ADB device found on USB.\n\
                 - enable Developer options -> USB debugging on the phone,\n\
                 - connect it over USB and accept the authorization prompt,\n\
                 - on Windows the phone's ADB interface must be bound to the WinUSB/Google driver."
            );
        }
        let mut last_err = None;
        let count = cands.len();
        for cand in cands {
            match Self::connect(cand) {
                Ok(dev) => return Ok(dev),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("failed to connect ({count} candidate(s))")))
    }

    fn connect(cand: Candidate) -> Result<AdbDevice> {
        let serial = cand.info.serial_number().map(|s| s.to_string());
        let key = AdbKey::load_or_create().context("load/create adbkey")?;
        let io = UsbTransport::open(&cand, 256 * 1024)?;
        let mut dev = AdbDevice {
            io,
            key,
            maxdata: MAX_PAYLOAD,
            next_local_id: 1,
            banner: Banner::default(),
            serial,
        };
        dev.handshake()?;
        Ok(dev)
    }

    fn write_msg(&self, m: &Message) -> Result<()> {
        self.io.write_all(m.header().to_vec())?;
        if !m.payload.is_empty() {
            self.io.write_all(m.payload.clone())?;
        }
        Ok(())
    }

    fn read_msg(&mut self) -> Result<Message> {
        let head = self.io.read_exact(HEADER_LEN)?;
        let h = decode_header(&head)?;
        let payload = if h.data_len > 0 {
            let p = self.io.read_exact(h.data_len as usize)?;
            let sum = checksum(&p);
            if sum != h.data_check {
                bail!(
                    "payload checksum mismatch ({:#x} != {:#x}) on {}",
                    sum,
                    h.data_check,
                    h.command_code()
                );
            }
            p
        } else {
            Vec::new()
        };
        Ok(Message::new(h.command, h.arg0, h.arg1, payload))
    }

    /// ADB CNXN/AUTH; reuses ~/.android/adbkey.
    fn handshake(&mut self) -> Result<()> {
        let banner = b"host::features=cmd,shell_v2\0".to_vec();
        self.write_msg(&Message::new(A_CNXN, A_VERSION, MAX_PAYLOAD, banner))?;

        let mut signature_sent = false;
        let mut pubkey_sent = false;
        loop {
            let msg = self.read_msg()?;
            match msg.command {
                A_CNXN => {
                    self.maxdata = msg.arg1.clamp(4096, MAX_PAYLOAD);
                    self.banner = parse_banner(&msg.payload);
                    return Ok(());
                }
                A_AUTH if msg.arg0 == AUTH_TOKEN => {
                    if !signature_sent {
                        let sig = self.key.sign_token(&msg.payload)?;
                        self.write_msg(&Message::new(A_AUTH, AUTH_SIGNATURE, 0, sig))?;
                        signature_sent = true;
                    } else if !pubkey_sent {
                        let line = self.key.android_pubkey_line("mmbuild@microminer")?;
                        self.write_msg(&Message::new(A_AUTH, AUTH_RSAPUBLICKEY, 0, line))?;
                        pubkey_sent = true;
                    } else {
                        bail!(
                            "device did not authorize the key — accept the \
                             'Allow USB debugging?' prompt on the phone and retry"
                        );
                    }
                }
                other => bail!(
                    "unexpected message during handshake: {} ({other:#010x})",
                    msg.code()
                ),
            }
        }
    }

    fn alloc_local_id(&mut self) -> u32 {
        let id = self.next_local_id;
        self.next_local_id = self.next_local_id.wrapping_add(1).max(1);
        id
    }

    /// Open a service stream and collect its output until close.
    fn open_collect(&mut self, destination: &str) -> Result<Vec<u8>> {
        let local_id = self.alloc_local_id();
        let mut dest = destination.as_bytes().to_vec();
        dest.push(0);
        self.write_msg(&Message::new(A_OPEN, local_id, 0, dest))?;

        // First reply establishes (or refuses) the stream.
        let remote_id;
        loop {
            let msg = self.read_msg()?;
            match msg.command {
                A_OKAY if msg.arg1 == local_id => {
                    remote_id = msg.arg0;
                    break;
                }
                A_CLSE if msg.arg1 == local_id => {
                    bail!("device refused to open stream {destination:?}");
                }
                A_OKAY | A_WRTE | A_CLSE => continue, // not ours yet
                other => bail!(
                    "unexpected {} opening {destination:?}",
                    Message::new(other, 0, 0, vec![]).code()
                ),
            }
        }

        let mut out = Vec::new();
        loop {
            let msg = self.read_msg()?;
            match msg.command {
                A_WRTE if msg.arg1 == local_id => {
                    out.extend_from_slice(&msg.payload);
                    // Acknowledge so the device keeps sending.
                    self.write_msg(&Message::new(A_OKAY, local_id, remote_id, vec![]))?;
                }
                A_CLSE if msg.arg1 == local_id => {
                    // Mirror the close and finish.
                    self.write_msg(&Message::new(A_CLSE, local_id, remote_id, vec![]))?;
                    break;
                }
                A_OKAY => continue, // flow-control ack for our writes
                _ => continue,      // traffic for another stream
            }
        }
        Ok(out)
    }

    /// Run a shell command and return its combined stdout/stderr as text.
    pub fn shell(&mut self, cmd: &str) -> Result<String> {
        let raw = self.open_collect(&format!("shell:{cmd}"))?;
        Ok(String::from_utf8_lossy(&raw).replace("\r\n", "\n"))
    }

    /// Read a (text) file off the device via `cat`.
    pub fn read_text_file(&mut self, path: &str) -> Result<String> {
        self.shell(&format!("cat '{path}' 2>/dev/null"))
    }

    /// Read a binary file off the device, base64-encoded over the shell.
    pub fn read_binary_file(&mut self, path: &str) -> Result<Vec<u8>> {
        let out = self.shell(&format!(
            "(toybox base64 '{path}' || base64 '{path}') 2>/dev/null"
        ))?;
        let compact: String = out.split_whitespace().collect();
        if compact.is_empty() {
            bail!("empty/again no base64 output for {path}");
        }
        base64::engine::general_purpose::STANDARD
            .decode(compact.as_bytes())
            .with_context(|| format!("decode base64 of {path}"))
    }
}

fn parse_banner(payload: &[u8]) -> Banner {
    let raw = String::from_utf8_lossy(payload)
        .trim_end_matches('\0')
        .to_string();
    // "<systemtype>:<serial>:<k=v;k=v;...>"
    let mut b = Banner {
        raw: raw.clone(),
        ..Banner::default()
    };
    if let Some(props) = raw.splitn(3, ':').nth(2) {
        for kv in props.split(';') {
            if let Some((k, v)) = kv.split_once('=') {
                let v = v.trim().to_string();
                match k.trim() {
                    "ro.product.name" => b.product = Some(v),
                    "ro.product.model" => b.model = Some(v),
                    "ro.product.device" => b.device = Some(v),
                    _ => {}
                }
            }
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_parsing() {
        let p = b"device::ro.product.name=a12;ro.product.model=SM-A125F;ro.product.device=a12;features=cmd\0";
        let b = parse_banner(p);
        assert_eq!(b.model.as_deref(), Some("SM-A125F"));
        assert_eq!(b.device.as_deref(), Some("a12"));
        assert_eq!(b.product.as_deref(), Some("a12"));
    }
}
