//! Reads what identifies an Xbox title from its `default.xbe`: the certificate
//! (title name, title ID, region, disc number) and the section table, which
//! says where the title image lives.
//!
//! Only the image header is needed for any of it, and the header sits at the
//! start of the file, so a caller reading over FTP fetches
//! [`headers_size`] bytes and stops there. The title image is a separate,
//! later read of the `$$XTIMAGE` section (see [`image`]).

pub mod image;

use serde::Serialize;

/// Bytes a caller must have before [`headers_size`] can answer.
pub const SIZE_FIELD_END: usize = 0x10C;

const MAGIC: &[u8; 4] = b"XBEH";
const SECTION_HEADER_SIZE: usize = 56;

pub const REGION_NA: u32 = 0x0000_0001;
pub const REGION_JAPAN: u32 = 0x0000_0002;
pub const REGION_REST_OF_WORLD: u32 = 0x0000_0004;
pub const REGION_MANUFACTURING: u32 = 0x8000_0000;

/// The section holding the title image shown by the dashboard.
pub const TITLE_IMAGE_SECTION: &str = "$$XTIMAGE";

#[derive(Debug, thiserror::Error)]
pub enum XbeError {
    #[error("not an XBE (no XBEH magic)")]
    BadMagic,
    #[error("XBE header is truncated: need {needed} bytes, have {have}")]
    Truncated { needed: usize, have: usize },
    #[error("XBE header field {0} points outside the header")]
    OutOfRange(&'static str),
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    pub name: String,
    pub raw_addr: u32,
    pub raw_size: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct XbeInfo {
    pub title_name: String,
    pub title_id: u32,
    pub game_region: u32,
    pub disc_number: u32,
    pub version: u32,
    pub sections: Vec<Section>,
}

impl XbeInfo {
    /// The title ID as the eight hex digits the scene and most databases use, e.g. `4D530004`.
    pub fn title_id_hex(&self) -> String {
        format!("{:08X}", self.title_id)
    }

    /// The title ID as printed on the disc, e.g. `MS-004`: two publisher letters from the high
    /// half-word and a number from the low one. `None` when the high half is not two printable
    /// characters (homebrew often leaves it zero).
    pub fn title_id_code(&self) -> Option<String> {
        let hi = (self.title_id >> 16) as u16;
        let a = (hi >> 8) as u8;
        let b = (hi & 0xFF) as u8;
        if a.is_ascii_graphic() && b.is_ascii_graphic() {
            Some(format!("{}{}-{:03}", a as char, b as char, self.title_id & 0xFFFF))
        } else {
            None
        }
    }

    /// The region as the short labels used in ISO names: `USA`, `Japan`, `Europe`, joined
    /// with `, ` when a title allows several, or `World` when it allows all three.
    pub fn region_label(&self) -> String {
        let r = self.game_region;
        let all = REGION_NA | REGION_JAPAN | REGION_REST_OF_WORLD;
        if r & all == all {
            return "World".to_string();
        }
        let mut parts = Vec::new();
        if r & REGION_NA != 0 {
            parts.push("USA");
        }
        if r & REGION_JAPAN != 0 {
            parts.push("Japan");
        }
        if r & REGION_REST_OF_WORLD != 0 {
            parts.push("Europe");
        }
        if parts.is_empty() {
            if r & REGION_MANUFACTURING != 0 {
                return "Debug".to_string();
            }
            return "Unknown".to_string();
        }
        parts.join(", ")
    }

    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }
}

fn u32_at(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn need(data: &[u8], needed: usize) -> Result<(), XbeError> {
    if data.len() < needed {
        Err(XbeError::Truncated { needed, have: data.len() })
    } else {
        Ok(())
    }
}

/// How many bytes from the start of the file hold the whole image header. Needs the first
/// [`SIZE_FIELD_END`] bytes.
pub fn headers_size(prefix: &[u8]) -> Result<usize, XbeError> {
    need(prefix, SIZE_FIELD_END)?;
    if &prefix[0..4] != MAGIC {
        return Err(XbeError::BadMagic);
    }
    Ok(u32_at(prefix, 0x108).unwrap() as usize)
}

/// Parses the certificate and the section table out of the image header. `data` must start at
/// the beginning of the file and hold at least [`headers_size`] bytes.
pub fn parse(data: &[u8]) -> Result<XbeInfo, XbeError> {
    headers_size(data)?;

    let base = u32_at(data, 0x104).unwrap();
    let to_off = |va: u32, what: &'static str| -> Result<usize, XbeError> {
        va.checked_sub(base)
            .map(|o| o as usize)
            .filter(|&o| o < data.len())
            .ok_or(XbeError::OutOfRange(what))
    };

    let cert_va = u32_at(data, 0x118).ok_or(XbeError::OutOfRange("certificate"))?;
    let cert = to_off(cert_va, "certificate")?;
    need(data, cert + 176)?;

    let name_bytes = &data[cert + 12..cert + 12 + 80];
    let units: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    let title_name = String::from_utf16_lossy(&units).trim().to_string();

    let num_sections = u32_at(data, 0x11C).unwrap() as usize;
    let sec_va = u32_at(data, 0x120).unwrap();
    let mut sections = Vec::with_capacity(num_sections);
    if num_sections > 0 {
        let sec_off = to_off(sec_va, "section headers")?;
        need(data, sec_off + num_sections * SECTION_HEADER_SIZE)?;
        for i in 0..num_sections {
            let off = sec_off + i * SECTION_HEADER_SIZE;
            let name_va = u32_at(data, off + 20).unwrap();
            let name = to_off(name_va, "section name")
                .map(|o| {
                    let end = data[o..].iter().position(|&b| b == 0).map_or(data.len(), |p| o + p);
                    String::from_utf8_lossy(&data[o..end.min(o + 64)]).into_owned()
                })
                .unwrap_or_default();
            sections.push(Section {
                name,
                raw_addr: u32_at(data, off + 12).unwrap(),
                raw_size: u32_at(data, off + 16).unwrap(),
            });
        }
    }

    Ok(XbeInfo {
        title_name,
        title_id: u32_at(data, cert + 8).unwrap(),
        game_region: u32_at(data, cert + 160).unwrap(),
        disc_number: u32_at(data, cert + 168).unwrap(),
        version: u32_at(data, cert + 172).unwrap(),
        sections,
    })
}

/// Builds a minimal XBE header for tests: a certificate and an optional `$$XTIMAGE` section
/// whose raw data starts at `image_at`. Not a runnable executable.
#[doc(hidden)]
pub fn synthetic(title: &str, title_id: u32, region: u32, image: Option<(u32, u32)>) -> Vec<u8> {
    const BASE: u32 = 0x10000;
    const HEADERS: usize = 0x1000;
    let mut d = vec![0u8; HEADERS];
    let put = |d: &mut Vec<u8>, off: usize, v: u32| d[off..off + 4].copy_from_slice(&v.to_le_bytes());

    d[0..4].copy_from_slice(MAGIC);
    put(&mut d, 0x104, BASE);
    put(&mut d, 0x108, HEADERS as u32);

    let cert = 0x400usize;
    put(&mut d, 0x118, BASE + cert as u32);
    put(&mut d, cert, 0x1D0);
    put(&mut d, cert + 8, title_id);
    for (i, u) in title.encode_utf16().take(40).enumerate() {
        d[cert + 12 + i * 2..cert + 14 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    put(&mut d, cert + 160, region);
    put(&mut d, cert + 168, 0);

    let sec = 0x200usize;
    let names = 0x300usize;
    let mut n = 0u32;
    if let Some((raw_addr, raw_size)) = image {
        let name = TITLE_IMAGE_SECTION.as_bytes();
        d[names..names + name.len()].copy_from_slice(name);
        put(&mut d, sec + 12, raw_addr);
        put(&mut d, sec + 16, raw_size);
        put(&mut d, sec + 20, BASE + names as u32);
        n = 1;
    }
    put(&mut d, 0x11C, n);
    put(&mut d, 0x120, BASE + sec as u32);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_certificate_and_sections() {
        let d = synthetic("Halo: Combat Evolved", 0x4D53_0004, REGION_NA, Some((0x2000, 0x800)));
        assert_eq!(headers_size(&d).unwrap(), 0x1000);
        let x = parse(&d).unwrap();
        assert_eq!(x.title_name, "Halo: Combat Evolved");
        assert_eq!(x.title_id_hex(), "4D530004");
        assert_eq!(x.title_id_code().as_deref(), Some("MS-004"));
        assert_eq!(x.region_label(), "USA");
        let s = x.section(TITLE_IMAGE_SECTION).unwrap();
        assert_eq!((s.raw_addr, s.raw_size), (0x2000, 0x800));
    }

    #[test]
    fn region_labels() {
        let mut x = parse(&synthetic("T", 1, 7, None)).unwrap();
        assert_eq!(x.region_label(), "World");
        x.game_region = REGION_JAPAN | REGION_REST_OF_WORLD;
        assert_eq!(x.region_label(), "Japan, Europe");
        x.game_region = REGION_MANUFACTURING;
        assert_eq!(x.region_label(), "Debug");
    }

    #[test]
    fn rejects_non_xbe() {
        let d = vec![0u8; 0x200];
        assert!(matches!(headers_size(&d), Err(XbeError::BadMagic)));
        assert!(matches!(headers_size(&d[..8]), Err(XbeError::Truncated { .. })));
    }

    #[test]
    fn homebrew_title_id_has_no_code() {
        let x = parse(&synthetic("Homebrew", 0x0000_0001, 7, None)).unwrap();
        assert_eq!(x.title_id_code(), None);
    }
}
