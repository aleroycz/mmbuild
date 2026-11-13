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

//! Samsung FUS HTTP client: Smart Download nonce, version lookup, encrypted download.
//! Protocol aligned with samloader-fus 2.2 / SamFirm_Reborn >=0.3.7.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::crypt;

const FUS_BASE: &str = "https://neofussvr.sslcs.cdngc.net/";
const CLOUD_DL: &str = "http://cloud-neofussvr.samsungmobile.com/NF_SmartDownloadBinaryForMass.do";
const VERSION_BASE: &str = "https://fota-cloud-dn.ospserver.net/firmware";
const UA: &str = "SMART 2.0";

/// What a BinaryInform response tells us about the firmware blob.
#[derive(Debug, Clone)]
pub struct BinaryInfo {
    pub filename: String,
    pub size: u64,
    pub model_path: String,
    pub fw_version: String,
    pub logic_value: String,
    pub model_type: String,
    pub region: String,
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
    /// Raw NONCE header - used for LOGIC_CHECK and Authorization nonce=.
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
        c.makereq("NF_SmartDownloadGenerateNonce.do", "")?;
        if c.enc_nonce.is_empty() || c.auth.is_empty() {
            bail!("FUS GenerateNonce returned no usable NONCE header");
        }
        Ok(c)
    }

    fn auth_header(&self) -> String {
        format!(
            "FUS nonce=\"{nonce}\", signature=\"{sig}\", nc=\"\", type=\"\", realm=\"\", newauth=\"1\"",
            nonce = self.enc_nonce,
            sig = self.auth
        )
    }

    fn makereq(&mut self, path: &str, data: &str) -> Result<String> {
        let url = format!("{FUS_BASE}{path}");
        let auth = if self.enc_nonce.is_empty() {
            "FUS nonce=\"\", signature=\"\", nc=\"\", type=\"\", realm=\"\", newauth=\"1\""
                .to_string()
        } else {
            self.auth_header()
        };
        let mut req = self
            .agent
            .post(&url)
            .set("Authorization", &auth)
            .set("User-Agent", UA)
            .set("Cache-Control", "no-cache");
        if !self.sessid.is_empty() {
            req = req.set("Cookie", &format!("JSESSIONID={}", self.sessid));
        }
        let resp = if data.is_empty() {
            req.send_string("")
        } else {
            let compact: String = data
                .chars()
                .filter(|c| *c != '\r' && *c != '\n' && *c != '\t')
                .collect();
            req.set("Content-Type", "application/xml")
                .send_string(&compact)
        }
        .with_context(|| format!("FUS POST {path}"))?;

        if let Some(enc) = resp.header("NONCE").or_else(|| resp.header("nonce")) {
            if !enc.is_empty() && enc != self.enc_nonce {
                self.enc_nonce = enc.to_string();
                self.nonce = crypt::session_nonce(enc)?;
                self.auth = crypt::get_auth(enc)?;
            }
        }
        for sc in resp.all("Set-Cookie") {
            for part in sc.split(';') {
                let p = part.trim();
                if let Some(v) = p
                    .strip_prefix("JSESSIONID=")
                    .or_else(|| p.strip_prefix("jsessionid="))
                {
                    self.sessid = v.to_string();
                }
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
            .set("User-Agent", UA)
            .call()
            .with_context(|| format!("version.xml {model}/{region}"))?
            .into_string()?;
        let latest =
            xml_tag(&xml, "latest").context("no <latest> in version.xml (model/region wrong?)")?;
        Ok(normalize_vercode(&latest))
    }

    /// BinaryInform: resolve the download filename, size, path and decrypt keys.
    pub fn binary_inform(&mut self, fw: &str, model: &str, region: &str) -> Result<BinaryInfo> {
        let logic = crypt::logic_check(fw, &self.nonce)?;
        let body = build_inform_xml(fw, model, region, &logic);
        let resp = self.makereq("NF_SmartDownloadBinaryInform.do", &body)?;
        let status = xml_tag(&resp, "Status").unwrap_or_default();
        if status != "200" && status != "S00" {
            bail!(
                "BinaryInform status {status} (model/region/version not available, \
                 or FUS rejected Authorization)"
            );
        }
        let filename = xml_data(&resp, "BINARY_NAME").context("no BINARY_NAME")?;
        let size = xml_data(&resp, "BINARY_BYTE_SIZE")
            .and_then(|s| s.parse::<u64>().ok())
            .context("no BINARY_BYTE_SIZE")?;
        let model_path = xml_data(&resp, "MODEL_PATH").unwrap_or_default();
        let fw_version = xml_data(&resp, "LATEST_FW_VERSION").unwrap_or_else(|| fw.to_string());
        let logic_value = xml_data(&resp, "LOGIC_VALUE_FACTORY").unwrap_or_default();
        let model_type = xml_data(&resp, "DEVICE_MODEL_TYPE").unwrap_or_else(|| "1".into());
        Ok(BinaryInfo {
            filename,
            size,
            model_path,
            fw_version,
            logic_value,
            model_type,
            region: region.to_string(),
        })
    }

    /// BinaryInit: arm the mass-download for the chosen file.
    pub fn binary_init(&mut self, info: &BinaryInfo) -> Result<()> {
        let start = info.filename.len().saturating_sub(25);
        let end = info.filename.len().saturating_sub(9);
        let check_in = if end > start {
            &info.filename[start..end]
        } else {
            info.filename.split('.').next().unwrap_or(&info.filename)
        };
        let logic = crypt::logic_check(check_in, &self.nonce)?;
        let body = build_init_xml(
            &info.filename,
            &info.fw_version,
            &info.region,
            &info.model_type,
            &logic,
        );
        self.makereq("NF_SmartDownloadBinaryInitForMass.do", &body)?;
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
        /* Build URL by hand so `/` in MODEL_PATH stays unescaped (Samsung 403s on %2F). */
        let url = format!("{CLOUD_DL}?file={remote_path}");
        let mut req = self
            .agent
            .get(&url)
            .set("Authorization", &self.auth_header())
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

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let gt = xml[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = xml[gt..].find(&close)? + gt;
    Some(xml[gt..end].trim().to_string())
}

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
    /* Matches samloader-fus binary_inform_req_xml. */
    format!(
        "<FUSMsg>\
<FUSHdr><ProtoVer>1.0</ProtoVer><SessionID>0</SessionID><MsgID>1</MsgID></FUSHdr>\
<FUSBody>\
<Put>\
<CmdID>1</CmdID>\
<ACCESS_MODE><Data>1</Data></ACCESS_MODE>\
<BINARY_NATURE><Data>1</Data></BINARY_NATURE>\
<REQUEST_TYPE><Data>2</Data></REQUEST_TYPE>\
<LOGIC_CHECK><Data>{logic}</Data></LOGIC_CHECK>\
<BINARY_SW_VERSION><Data>{fw}</Data></BINARY_SW_VERSION>\
<BINARY_LOCAL_CODE><Data>{region}</Data></BINARY_LOCAL_CODE>\
<BINARY_MODEL_NAME><Data>{model}</Data></BINARY_MODEL_NAME>\
</Put>\
<Get>\
<CmdID>2</CmdID>\
<BINARY_SW_VERSION></BINARY_SW_VERSION>\
</Get>\
</FUSBody>\
</FUSMsg>"
    )
}

fn build_init_xml(filename: &str, fw: &str, region: &str, model_type: &str, logic: &str) -> String {
    format!(
        "<FUSMsg>\
<FUSHdr><ProtoVer>1.0</ProtoVer><SessionID>0</SessionID><MsgID>1</MsgID></FUSHdr>\
<FUSBody><Put>\
<BINARY_NAME><Data>{filename}</Data></BINARY_NAME>\
<BINARY_SW_VERSION><Data>{fw}</Data></BINARY_SW_VERSION>\
<DEVICE_LOCAL_CODE><Data>{region}</Data></DEVICE_LOCAL_CODE>\
<DEVICE_MODEL_TYPE><Data>{model_type}</Data></DEVICE_MODEL_TYPE>\
<LOGIC_CHECK><Data>{logic}</Data></LOGIC_CHECK>\
</Put></FUSBody></FUSMsg>"
    )
}
