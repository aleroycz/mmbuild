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

//! Resolve latest Google factory image URL for a Pixel codename.

use anyhow::{Context, Result};

const INDEX: &str = "https://developers.google.com/android/images";
/// Direct-download prefix for every factory image.
const DL_PREFIX: &str = "https://dl.google.com/dl/android/aosp/";
/// Consent cookie the page requires before it exposes the links.
const TOS_COOKIE: &str = "devsite_wall_acks=nexus-image-tos";

/// Resolve the newest factory-image URL for a Pixel codename (e.g. "oriole").
pub fn resolve_factory_url(codename: &str) -> Result<String> {
    let html = ureq::get(INDEX)
        .set("Cookie", TOS_COOKIE)
        .call()
        .context("GET developers.google.com/android/images")?
        .into_string()
        .context("read factory-images page")?;
    pick_factory_url(&html, codename).with_context(|| {
        format!(
            "no Pixel factory image found for codename {codename:?} on {INDEX} \
             (check the codename, or set [firmware].url to a direct link)"
        )
    })
}

/// Newest factory zip URL from HTML (unit-testable).
fn pick_factory_url(html: &str, codename: &str) -> Option<String> {
    let prefix = format!("{codename}-");
    let mut best: Option<String> = None;
    for (idx, _) in html.match_indices(DL_PREFIX) {
        let tail = &html[idx..];
        let end = match tail.find(".zip") {
            Some(e) => e + 4,
            None => continue,
        };
        let url = &tail[..end];
        let file = url.rsplit('/').next().unwrap_or("");
        // e.g. oriole-ap4a.250105.002-factory-abcd1234.zip
        if file.starts_with(&prefix) && file.contains("-factory-") {
            best = Some(url.to_string()); // table lists oldest->newest; keep last
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
      <tr id="oriole"><td>oriole</td></tr>
      <tr><td>12.0.0 (SD1A.210817.015.A4)</td>
        <td><a href="https://dl.google.com/dl/android/aosp/oriole-sd1a.210817.015.a4-factory-aaaa1111.zip">Link</a></td></tr>
      <tr><td>15.0.0 (AP4A.250105.002)</td>
        <td><a href="https://dl.google.com/dl/android/aosp/oriole-ap4a.250105.002-factory-bbbb2222.zip">Link</a></td></tr>
      <tr id="raven"><td>raven</td></tr>
      <tr><td><a href="https://dl.google.com/dl/android/aosp/raven-ap4a.250105.002-factory-cccc3333.zip">Link</a></td></tr>
    "#;

    #[test]
    fn picks_newest_for_codename() {
        let got = pick_factory_url(SAMPLE, "oriole").unwrap();
        assert!(
            got.ends_with("oriole-ap4a.250105.002-factory-bbbb2222.zip"),
            "{got}"
        );
    }

    #[test]
    fn does_not_cross_codenames() {
        let got = pick_factory_url(SAMPLE, "raven").unwrap();
        assert!(got.contains("raven-"), "{got}");
        assert!(!got.contains("oriole"), "{got}");
    }

    #[test]
    fn unknown_codename_is_none() {
        assert!(pick_factory_url(SAMPLE, "tangorpro").is_none());
    }

    #[test]
    fn ota_zips_are_not_mistaken_for_factory() {
        // An OTA link (no "-factory-") must be ignored.
        let html = "https://dl.google.com/dl/android/aosp/oriole-ota-ap4a.250105.002-dddd.zip";
        assert!(pick_factory_url(html, "oriole").is_none());
    }
}
