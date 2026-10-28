//! Deterministic native-object bundling. Both linker indexes follow Microsoft's
//! PE/COFF Archive (Library) File Format, including endian order and alignment:
//! https://learn.microsoft.com/en-us/windows/win32/debug/pe-format#archive-library-file-format
//! Objects/exports come only from ownership-checked native generation.
use crate::{Error, Result};
use std::collections::BTreeMap;
pub(crate) struct Member {
    pub bytes: Vec<u8>,
    pub symbols: Vec<String>,
}
fn error() -> Error {
    Error::new(
        0,
        "native archive exceeds COFF limits or has invalid exports",
    )
}
/// Only the declared optimized C path accepts Microsoft's anonymous /GL
/// object format. Validate its exact class ID and bounded payload; it is not an
/// import-object header merely because both start with 00 00 ff ff.
pub(crate) fn object_payload(bytes: &[u8], allow_ltcg: bool) -> bool {
    let u16_at = |at| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
    };
    let u32_at = |at| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    };
    if allow_ltcg
        && bytes.get(..8) == Some(&[0, 0, 255, 255, 1, 0, 0x64, 0x86][..])
        && bytes.get(12..28)
            == Some(
                &[
                    0x38, 0xfe, 0xb3, 0x0c, 0xa5, 0xd9, 0xab, 0x4d, 0xac, 0x9b, 0xd6, 0xb6, 0x22,
                    0x26, 0x53, 0xc2,
                ][..],
            )
    {
        return bytes.len() > 32 && u32_at(28) == Some(bytes.len() - 32);
    }
    let (header, sections, symbol_offset, symbol_count, symbol_width) =
        if u16_at(0) == Some(0x8664) && u16_at(16) == Some(0) {
            (
                20,
                u16_at(2).unwrap_or(0) as usize,
                u32_at(8).unwrap_or(0),
                u32_at(12).unwrap_or(0),
                18,
            )
        } else if bytes.get(..8) == Some(&[0, 0, 255, 255, 2, 0, 0x64, 0x86][..])
            && bytes.get(12..28)
                == Some(
                    &[
                        0xc7, 0xa1, 0xba, 0xd1, 0xee, 0xba, 0xa9, 0x4b, 0xaf, 0x20, 0xfa, 0xf6,
                        0x6a, 0xa4, 0xdc, 0xb8,
                    ][..],
                )
        {
            (
                56,
                u32_at(44).unwrap_or(0),
                u32_at(48).unwrap_or(0),
                u32_at(52).unwrap_or(0),
                20,
            )
        } else {
            return false;
        };
    if sections == 0 || sections > bytes.len().saturating_sub(header) / 40 {
        return false;
    }
    if symbol_count > 0
        && (symbol_offset == 0
            || symbol_offset > bytes.len()
            || symbol_count > (bytes.len() - symbol_offset) / symbol_width)
    {
        return false;
    }
    for section in 0..sections {
        let at = header + section * 40;
        let size = u32_at(at + 16).unwrap();
        let offset = u32_at(at + 20).unwrap();
        // BSS has a virtual length and no file range.
        if offset != 0 && (offset > bytes.len() || size > bytes.len() - offset) {
            return false;
        }
        let relocations = u16_at(at + 32).unwrap() as usize;
        let offset = u32_at(at + 24).unwrap();
        if relocations != 0
            && (offset == 0 || offset > bytes.len() || relocations > (bytes.len() - offset) / 10)
        {
            return false;
        }
    }
    true
}
/// Validate the bounded archive envelope and x64 member format before any
/// provider reaches the linker or a whole-output receipt can be reused.
pub(crate) fn validate_provider(bytes: &[u8]) -> Result<()> {
    let invalid = || Error::new(0, "FFI provider is not a valid Windows x64 COFF archive");
    if !bytes.starts_with(b"!<arch>\n") {
        return Err(invalid());
    }
    let mut offset = 8usize;
    let mut objects = 0;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset + 60).ok_or_else(invalid)?;
        if &header[58..60] != b"`\n" {
            return Err(invalid());
        }
        let length = std::str::from_utf8(&header[48..58])
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .ok_or_else(invalid)?;
        let start = offset.checked_add(60).ok_or_else(invalid)?;
        let end = start.checked_add(length).ok_or_else(invalid)?;
        let body = bytes.get(start..end).ok_or_else(invalid)?;
        let name = std::str::from_utf8(&header[..16])
            .map_err(|_| invalid())?
            .trim();
        if name != "/" && name != "//" && name != "/SYM64/" {
            let import = body.len() > 22
                && body.get(..8) == Some(&[0, 0, 255, 255, 0, 0, 0x64, 0x86][..])
                && u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize == body.len() - 20
                && body[20..].iter().filter(|&&b| b == 0).count() >= 2;
            if !object_payload(body, true) && !import {
                return Err(invalid());
            }
            objects += 1;
        }
        offset = end.checked_add(length % 2).ok_or_else(invalid)?;
    }
    if offset != bytes.len() || objects == 0 {
        return Err(invalid());
    }
    Ok(())
}
fn member(out: &mut Vec<u8>, name: &str, data: &[u8]) -> Result<()> {
    let header = format!(
        "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
        0,
        "",
        "",
        "0",
        data.len()
    );
    if header.len() != 60 {
        return Err(error());
    }
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        out.push(b'\n');
    }
    Ok(())
}
pub(crate) fn build(members: &[Member]) -> Result<Vec<u8>> {
    if members.is_empty() || members.len() > u16::MAX as usize {
        return Err(error());
    }
    let mut symbols = BTreeMap::<&str, usize>::new();
    let mut string_bytes = 0u64;
    for (i, m) in members.iter().enumerate() {
        if m.bytes.len() > u32::MAX as usize {
            return Err(error());
        }
        for name in &m.symbols {
            if name.is_empty()
                || !name.is_ascii()
                || name.as_bytes().contains(&0)
                || symbols.insert(name, i).is_some()
            {
                return Err(error());
            }
            string_bytes = string_bytes
                .checked_add(name.len() as u64 + 1)
                .ok_or_else(error)?;
        }
    }
    let count = u32::try_from(symbols.len()).map_err(|_| error())?;
    let first_size = (4 + 4 * u64::from(count))
        .checked_add(string_bytes)
        .ok_or_else(error)?;
    let second_size = (8 + 4 * members.len() as u64 + 2 * u64::from(count))
        .checked_add(string_bytes)
        .ok_or_else(error)?;
    if first_size > u32::MAX as u64 || second_size > u32::MAX as u64 {
        return Err(error());
    }
    let padded = |n: u64| n + 60 + n % 2;
    let mut position = 8u64
        .checked_add(padded(first_size))
        .and_then(|p| p.checked_add(padded(second_size)))
        .ok_or_else(error)?;
    let mut offsets = Vec::new();
    for m in members {
        offsets.push(u32::try_from(position).map_err(|_| error())?);
        position = position
            .checked_add(padded(m.bytes.len() as u64))
            .ok_or_else(error)?;
    }
    if position > u32::MAX as u64 {
        return Err(error());
    }
    let mut first = Vec::new();
    first.extend_from_slice(&count.to_be_bytes());
    for (i, m) in members.iter().enumerate() {
        for _ in &m.symbols {
            first.extend_from_slice(&offsets[i].to_be_bytes());
        }
    }
    for m in members {
        for symbol in &m.symbols {
            first.extend_from_slice(symbol.as_bytes());
            first.push(0);
        }
    }
    let mut second = Vec::new();
    second.extend_from_slice(&(members.len() as u32).to_le_bytes());
    for offset in &offsets {
        second.extend_from_slice(&offset.to_le_bytes());
    }
    second.extend_from_slice(&count.to_le_bytes());
    for i in symbols.values() {
        second.extend_from_slice(&((*i + 1) as u16).to_le_bytes());
    }
    for symbol in symbols.keys() {
        second.extend_from_slice(symbol.as_bytes());
        second.push(0);
    }
    let mut out = b"!<arch>\n".to_vec();
    member(&mut out, "/", &first)?;
    member(&mut out, "/", &second)?;
    for (i, m) in members.iter().enumerate() {
        if out.len() != offsets[i] as usize {
            return Err(error());
        }
        member(&mut out, &format!("unit{i:04}.obj/"), &m.bytes)?;
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexes_use_correct_endianness_member_offsets_and_symbol_order() {
        let members = [
            Member {
                bytes: vec![1, 2, 3],
                symbols: vec!["zebra".into()],
            },
            Member {
                bytes: vec![4, 5],
                symbols: vec!["alpha".into()],
            },
        ];
        let a = build(&members).unwrap();
        assert_eq!(&a[..8], b"!<arch>\n");
        let first = 68;
        assert_eq!(
            u32::from_be_bytes(a[first..first + 4].try_into().unwrap()),
            2
        );
        let off0 = u32::from_be_bytes(a[first + 4..first + 8].try_into().unwrap()) as usize;
        let off1 = u32::from_be_bytes(a[first + 8..first + 12].try_into().unwrap()) as usize;
        assert_eq!(&a[off0..off0 + 13], b"unit0000.obj/");
        assert_eq!(&a[off0 + 60..off0 + 63], &[1, 2, 3]);
        assert_eq!(a[off0 + 63], b'\n');
        assert_eq!(off1, off0 + 64);
        let length = std::str::from_utf8(&a[56..66])
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        let second = 68 + length + length % 2 + 60;
        assert_eq!(
            u32::from_le_bytes(a[second..second + 4].try_into().unwrap()),
            2
        );
        assert_eq!(
            u32::from_le_bytes(a[second + 4..second + 8].try_into().unwrap()) as usize,
            off0
        );
        assert_eq!(
            u32::from_le_bytes(a[second + 8..second + 12].try_into().unwrap()) as usize,
            off1
        );
        assert_eq!(&a[second + 16..second + 20], &[2, 0, 1, 0]);
        assert_eq!(&a[second + 20..second + 32], b"alpha\0zebra\0");
    }
    #[test]
    fn archive_rejects_ambiguous_and_invalid_exports() {
        for symbols in [
            vec!["duplicate".into(), "duplicate".into()],
            vec!["bad\0name".into()],
            vec![String::new()],
        ] {
            assert!(build(&[Member {
                bytes: vec![],
                symbols
            }])
            .is_err());
        }
        assert!(build(&[]).is_err());
    }
}
