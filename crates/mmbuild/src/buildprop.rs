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

//! Read device model/SoC from firmware build.prop (not hardcoded).

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use superimg::ext4::Ext4;

#[derive(Debug, Clone, Default)]
pub struct DeviceProps {
    pub props: BTreeMap<String, String>,
}

impl DeviceProps {
    pub fn parse(text: &str) -> DeviceProps {
        let mut props = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                props.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        DeviceProps { props }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(|s| s.as_str())
    }

    /// First present value among several candidate keys.
    fn first(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().find_map(|k| self.get(k))
    }

    /// Marketing/model code, e.g. "SM-A125F".
    pub fn model(&self) -> Option<&str> {
        self.first(&[
            "ro.product.system.model",
            "ro.product.model",
            "ro.product.vendor.model",
        ])
    }
    /// Device codename, e.g. "a12".
    pub fn device(&self) -> Option<&str> {
        self.first(&[
            "ro.product.system.device",
            "ro.product.device",
            "ro.product.vendor.device",
            "ro.build.product",
        ])
    }
    /// SoC / board platform, e.g. "mt6765" - the key for SoC-based compat.
    pub fn platform(&self) -> Option<&str> {
        self.first(&["ro.board.platform", "ro.hardware", "ro.chipname"])
    }

    /// A safe filename stem for output artifacts (model, else device).
    pub fn output_stem(&self) -> String {
        let raw = self.model().or_else(|| self.device()).unwrap_or("device");
        raw.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

/// Read and parse build.prop from an opened system ext4 (tries common paths).
pub fn read_from_ext4(fs: &mut Ext4) -> Result<DeviceProps> {
    for path in [
        "/system/build.prop",
        "/build.prop",
        "/system/system/build.prop",
    ] {
        if let Ok(data) = fs.read_file(path) {
            return Ok(DeviceProps::parse(&String::from_utf8_lossy(&data)));
        }
    }
    bail!("build.prop not found in system partition (looked in /system/build.prop, /build.prop)");
}
