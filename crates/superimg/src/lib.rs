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

//! `superimg` — a self-contained toolkit for Android `super` / `system` images.
//!
//! Pure Rust, no loop-mount and no external `img2simg` / `lz4` / `tar`, so it
//! runs the same on Linux and Windows. Pieces:
//!   * [`sparse`]  — Android sparse ↔ raw (`img2simg` / `simg2img`)
//!   * [`lz4frame`] — standard lz4 frame (de)compression for Samsung `.lz4`
//!   * [`lp`]      — dynamic-partition (liblp) metadata reader
//!   * [`ext4`]    — ext4 reader + in-place patcher
//!   * [`tarball`] — Odin-compatible ustar writer

pub mod bootimg;
pub mod cpio;
pub mod ext4;
pub mod lp;
pub mod lz4frame;
pub mod sparse;
pub mod tarball;

pub use anyhow::{Context, Result};
