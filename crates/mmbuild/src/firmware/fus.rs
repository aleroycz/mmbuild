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

//! Samsung FUS API client (nonce handshake, version lookup, binary inform/init,
//! and the mass-download endpoint). Reimplements the samloader request flow with
//! `ureq`. Network-untested in this build environment; the local-file path is the
//! validated default and the configured fallback.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::crypt;

const FUS_BASE: &str = "https://neofussvr.sslcs.cdngc.net/";
const CLOUD_DL: &str = "http://cloud-neofussvr.sslcs.cdngc.net/NF_DownloadBinaryForMass.do";
const VERSION_BASE: &str = "https://fota-cloud-dn.ospserver.net/firmware";
const UA: &str = "Kies2.0_FUS";

/// What a BinaryInform response tells us about the firmware blob.
#[derive(Debug, Clone)]
pub struct BinaryInfo {
    pub filename: String,
    pub size: u64,
    pub model_path: String,
    pub fw_version: String,
    pub logic_value: String,
}

impl BinaryInfo {
    pub fn is_enc2(&self) -> bool {
        self.filename.ends_with(".enc2")
    }
}

pub struct FusClient {
    agent: ureq::Agent,
    auth: String,
    sessid: String,
    nonce: String,
    enc_nonce: String,
}

impl FusClient {
    pub fn new() -> Result<FusClient> {
        let mut c = FusClient {
            agent: ureq::AgentBuilder::new().build(),
            auth: String::new(),
            sessid: String::new(),
            nonce: String::new(),
            enc_nonce: String::new(),
        };
        c.makereq("NF_DownloadGenerateNonce.do", "")?; // prime the nonce
        Ok(c)
    }

    fn auth_header(&self, nonce_field: &str) -> String {
        format!(
            "FUS nonce=\"{nonce}\", signature=\"{sig}\", nc=\"\", type=\"\", realm=\"\", newauth=\"1\"",
            nonce = nonce_field,
            sig = self.auth
        )
    }

    fn makereq(&mut self, path: &str, data: &str) -> Result<String> {
        let url = format!("{FUS_BASE}{path}");
        let req = self
            .agent
            .post(&url)
            .set("Authorization", &self.auth_header(""))
            .set("User-Agent", UA)
            .set("Cookie", &format!("JSESSIONID={}", self.sessid));
        let resp = req
            .send_string(data)
            .with_context(|| format!("FUS POST {path}"))?;

        if let Some(enc) = resp.header("NONCE") {
            if !enc.is_empty() {
                self.enc_nonce = enc.to_string();
                self.nonce = crypt::decrypt_nonce(enc)?;
                self.auth = crypt::get_auth(&self.nonce)?;
            }
        }
        for sc in resp.all("Set-Cookie") {
            if let Some(v) = sc.strip_prefix("JSESSIONID=") {
                self.sessid = v.split(';').next().unwrap_or("").to_string();
            }
        }
        Ok(resp.into_string()?)
    }

    /// Fetch the latest firmware version code for a model/region.
    pub fn latest_version(&self, model: &str, region: &str) -> Result<String> {
        let url = format!("{VERSION_BASE}/{region}/{model}/version.xml");
        let xml = self
            .agent
            .get(&url)
            .call()
            .with_context(|| format!("version.xml {model}/{region}"))?
            .into_string()?;
        let latest = xml_tag(&xml, "latest")
            .context("no <latest> in version.xml (model/region wrong?)")?;
        Ok(normalize_vercode(&latest))
    }

    /// BinaryInform: resolve the download filename, size, path and decrypt keys.
    pub fn binary_inform(&mut self, fw: &str, model: &str, region: &str) -> Result<BinaryInfo> {
        let logic = crypt::logic_check(fw, &self.nonce)?;
        let body = build_inform_xml(fw, model, region, &logic);
        let resp = self.makereq("NF_DownloadBinaryInform.do", &body)?;
        let status = xml_tag(&resp, "Status").unwrap_or_default();
        if status != "200" {
            bail!("BinaryInform status {status} (model/region/version not available)");
        }
        let filename = xml_data(&resp, "BINARY_NAME").context("no BINARY_NAME")?;
        let size = xml_data(&resp, "BINARY_BYTE_SIZE")
            .and_then(|s| s.parse::<u64>().ok())
            .context("no BINARY_BYTE_SIZE")?;
        let model_path = xml_data(&resp, "MODEL_PATH").unwrap_or_default();
        let fw_version = xml_data(&resp, "LATEST_FW_VERSION").unwrap_or_else(|| fw.to_string());
        let logic_value = xml_data(&resp, "LOGIC_VALUE_FACTORY").unwrap_or_default();
        Ok(BinaryInfo {
            filename,
            size,
            model_path,
            fw_version,
            logic_value,
        })
    }

    /// BinaryInit: arm the mass-download for the chosen file.
    pub fn binary_init(&mut self, filename: &str) -> Result<()> {
        let stem = filename.split('.').next().unwrap_or(filename);
        let check_in: String = stem.chars().rev().take(16).collect::<Vec<_>>().into_iter().rev().collect();
        let logic = crypt::logic_check(&check_in, &self.nonce)?;
        let body = build_init_xml(filename, &logic);
        self.makereq("NF_DownloadBinaryInitForMass.do", &body)?;
        Ok(())
    }

    /// Stream the firmware blob to `out`, resuming from its current size.
    pub fn download(
        &self,
        remote_path: &str,
        out: &Path,
        expected_size: u64,
        mut progress: Option<&mut dyn FnMut(u64, u64)>,
    ) -> Result<()> {
        let start = out.metadata().map(|m| m.len()).unwrap_or(0);
        if start >= expected_size && expected_size > 0 {
            return Ok(());
        }
        let mut req = self
            .agent
            .get(CLOUD_DL)
            .query("file", remote_path)
            .set("Authorization", &self.auth_header(&self.enc_nonce))
            .set("User-Agent", UA);
        if start > 0 {
            req = req.set("Range", &format!("bytes={start}-"));
        }
        let resp = req.call().context("FUS mass download")?;
        let mut reader = resp.into_reader();
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(start > 0)
            .write(true)
            .open(out)?;
        let mut buf = vec![0u8; 1 << 16];
        let mut done = start;
        use std::io::Read;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n])?;
            done += n as u64;
            if let Some(cb) = progress.as_deref_mut() {
                cb(done, expected_size.max(done));
            }
        }
        f.flush()?;
        Ok(())
    }
}

// ---- tiny XML extraction (avoids pulling an XML crate) ----

/// Return the text of the first `<tag ...>text</tag>`.
fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let gt = xml[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = xml[gt..].find(&close)? + gt;
    Some(xml[gt..end].trim().to_string())
}

/// Return `<TAG><Data>value</Data></TAG>` inner value.
fn xml_data(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let close = format!("</{tag}>");
    let end = xml[start..].find(&close)? + start;
    let inner = &xml[start..end];
    xml_tag(inner, "Data").or_else(|| Some(inner.trim().to_string()))
}

fn normalize_vercode(vc: &str) -> String {
    let mut parts: Vec<String> = vc.split('/').map(|s| s.to_string()).collect();
    if parts.len() == 3 {
        parts.push(parts[0].clone());
    }
    if parts.len() >= 3 && parts[2].is_empty() {
        parts[2] = parts[0].clone();
    }
    parts.join("/")
}

fn build_inform_xml(fw: &str, model: &str, region: &str, logic: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><FUSMsg><FUSHdr><ProtoVer>1.0</ProtoVer></FUSHdr><FUSBody><Put>\
<ACCESS_MODE><Data>2</Data></ACCESS_MODE>\
<BINARY_NATURE><Data>1</Data></BINARY_NATURE>\
<CLIENT_PRODUCT><Data>Smart Switch</Data></CLIENT_PRODUCT>\
<DEVICE_FW_VERSION><Data>{fw}</Data></DEVICE_FW_VERSION>\
<DEVICE_LOCAL_CODE><Data>{region}</Data></DEVICE_LOCAL_CODE>\
<DEVICE_MODEL_NAME><Data>{model}</Data></DEVICE_MODEL_NAME>\
<LOGIC_CHECK><Data>{logic}</Data></LOGIC_CHECK>\
</Put></FUSBody></FUSMsg>"
    )
}

fn build_init_xml(filename: &str, logic: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><FUSMsg><FUSHdr><ProtoVer>1.0</ProtoVer></FUSHdr><FUSBody><Put>\
<BINARY_FILE_NAME><Data>{filename}</Data></BINARY_FILE_NAME>\
<LOGIC_CHECK><Data>{logic}</Data></LOGIC_CHECK>\
</Put></FUSBody></FUSMsg>"
    )
}
