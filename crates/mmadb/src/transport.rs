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

//! ADB USB bulk IN/OUT via nusb (class 0xff/0x42/0x01).

use anyhow::{anyhow, bail, Context, Result};
use futures_lite::future::block_on;
use nusb::transfer::{Direction, EndpointType, RequestBuffer};
use nusb::{DeviceInfo, Interface};

pub const ADB_CLASS: u8 = 0xff;
pub const ADB_SUBCLASS: u8 = 0x42;
pub const ADB_PROTOCOL: u8 = 0x01;

pub struct UsbTransport {
    interface: Interface,
    ep_in: u8,
    ep_out: u8,
    read_chunk: usize,
    buf: Vec<u8>,
}

/// A candidate ADB device discovered on the bus.
pub struct Candidate {
    pub info: DeviceInfo,
    pub interface_number: u8,
    pub ep_in: u8,
    pub ep_out: u8,
}

impl UsbTransport {
    /// Enumerate the bus and return every device exposing an ADB interface.
    pub fn list() -> Result<Vec<Candidate>> {
        let mut out = Vec::new();
        for info in nusb::list_devices().context("enumerate USB devices")? {
            let has_adb_iface = info.interfaces().any(|i| {
                i.class() == ADB_CLASS
                    && i.subclass() == ADB_SUBCLASS
                    && i.protocol() == ADB_PROTOCOL
            });
            if !has_adb_iface {
                continue;
            }
            match find_endpoints(&info) {
                Ok(Some((ifn, ep_in, ep_out))) => out.push(Candidate {
                    info,
                    interface_number: ifn,
                    ep_in,
                    ep_out,
                }),
                Ok(None) => {}
                Err(_) => {} // can't open (driver/permissions) - skip, not fatal
            }
        }
        Ok(out)
    }

    /// Claim a candidate's ADB interface and build a transport.
    pub fn open(cand: &Candidate, read_chunk: usize) -> Result<UsbTransport> {
        let device = cand.info.open().context("open USB device")?;
        let interface = device
            .claim_interface(cand.interface_number)
            .with_context(|| {
                format!(
                    "claim ADB interface {} (is another adb server holding it?)",
                    cand.interface_number
                )
            })?;
        Ok(UsbTransport {
            interface,
            ep_in: cand.ep_in,
            ep_out: cand.ep_out,
            read_chunk: read_chunk.max(512),
            buf: Vec::new(),
        })
    }

    /// Write a full buffer to the bulk OUT endpoint in one transfer.
    pub fn write_all(&self, data: Vec<u8>) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let comp = block_on(self.interface.bulk_out(self.ep_out, data));
        comp.status.map_err(|e| anyhow!("bulk OUT failed: {e:?}"))?;
        Ok(())
    }

    /// Read exactly `n` bytes, reassembling across bulk IN transfers.
    pub fn read_exact(&mut self, n: usize) -> Result<Vec<u8>> {
        while self.buf.len() < n {
            let want = (n - self.buf.len()).max(self.read_chunk);
            let comp = block_on(self.interface.bulk_in(self.ep_in, RequestBuffer::new(want)));
            comp.status.map_err(|e| anyhow!("bulk IN failed: {e:?}"))?;
            if comp.data.is_empty() {
                bail!("device closed the USB stream (zero-length read)");
            }
            self.buf.extend_from_slice(&comp.data);
        }
        let rest = self.buf.split_off(n);
        let out = std::mem::replace(&mut self.buf, rest);
        Ok(out)
    }
}

/// Find the ADB interface number and its bulk IN/OUT endpoint addresses.
fn find_endpoints(info: &DeviceInfo) -> Result<Option<(u8, u8, u8)>> {
    let device = info.open()?;
    let config = device
        .active_configuration()
        .map_err(|e| anyhow!("active configuration: {e:?}"))?;
    for alt in config.interface_alt_settings() {
        if alt.class() != ADB_CLASS
            || alt.subclass() != ADB_SUBCLASS
            || alt.protocol() != ADB_PROTOCOL
        {
            continue;
        }
        let mut ep_in = None;
        let mut ep_out = None;
        for ep in alt.endpoints() {
            if ep.transfer_type() != EndpointType::Bulk {
                continue;
            }
            match ep.direction() {
                Direction::In => ep_in = Some(ep.address()),
                Direction::Out => ep_out = Some(ep.address()),
            }
        }
        if let (Some(i), Some(o)) = (ep_in, ep_out) {
            return Ok(Some((alt.interface_number(), i, o)));
        }
    }
    Ok(None)
}
