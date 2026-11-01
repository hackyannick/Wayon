//! Platzsparende Kodierungen: Varints und Bitpacking für Integer-Blöcke.

/// Hängt `v` als LEB128-Varint an `out` an.
#[inline]
pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Liest einen Varint ab `*pos` und erhöht `*pos`.
/// Bei abgeschnittenen Daten wird 0 geliefert (der Aufrufer prüft Längen).
#[inline]
pub fn read_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v: u64 = 0;
    let mut shift = 0;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return v;
        }
        shift += 7;
        if shift > 63 {
            break;
        }
    }
    v
}

/// Überspringt `n` Varints ab `*pos`.
#[inline]
pub fn skip_varints(buf: &[u8], pos: &mut usize, mut n: usize) {
    while n > 0 && *pos < buf.len() {
        if buf[*pos] < 0x80 {
            n -= 1;
        }
        *pos += 1;
    }
}

pub fn write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn write_str(out: &mut Vec<u8>, s: &str) {
    write_bytes(out, s.as_bytes());
}

pub fn read_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> anyhow::Result<&'a [u8]> {
    let len = read_varint(buf, pos) as usize;
    let end = pos.checked_add(len).filter(|&e| e <= buf.len());
    match end {
        Some(end) => {
            let s = &buf[*pos..end];
            *pos = end;
            Ok(s)
        }
        None => anyhow::bail!("abgeschnittene Daten"),
    }
}

pub fn read_str<'a>(buf: &'a [u8], pos: &mut usize) -> anyhow::Result<&'a str> {
    Ok(std::str::from_utf8(read_bytes(buf, pos)?)?)
}

/// Anzahl Bits, die der größte Wert benötigt.
#[inline]
pub fn bits_needed(values: &[u32]) -> u8 {
    let max = values.iter().fold(0u32, |a, &b| a | b);
    (32 - max.leading_zeros()) as u8
}

/// Packt `values` mit fester Bitbreite: 1 Byte Breite + ceil(n*b/8) Bytes.
/// Blöcke, in denen alle Werte 0 sind, kosten nur 1 Byte.
pub fn pack(out: &mut Vec<u8>, values: &[u32]) {
    let b = bits_needed(values);
    out.push(b);
    if b == 0 {
        return;
    }
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    for &v in values {
        acc |= (v as u64) << nbits;
        nbits += b as u32;
        while nbits >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            nbits -= 8;
        }
    }
    if nbits > 0 {
        out.push(acc as u8);
    }
}

/// Entpackt `out.len()` Werte, Gegenstück zu [`pack`].
pub fn unpack(buf: &[u8], pos: &mut usize, out: &mut [u32]) {
    let b = buf.get(*pos).copied().unwrap_or(0) as u32;
    *pos += 1;
    if b == 0 {
        out.iter_mut().for_each(|v| *v = 0);
        return;
    }
    let mask: u64 = if b == 32 {
        u32::MAX as u64
    } else {
        (1u64 << b) - 1
    };
    let mut acc: u64 = 0;
    let mut nbits: u32 = 0;
    let mut p = *pos;
    for v in out.iter_mut() {
        while nbits < b {
            let byte = buf.get(p).copied().unwrap_or(0) as u64;
            acc |= byte << nbits;
            nbits += 8;
            p += 1;
        }
        *v = (acc & mask) as u32;
        acc >>= b;
        nbits -= b;
    }
    let total_bytes = (out.len() * b as usize).div_ceil(8);
    *pos += total_bytes;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        let vals = [0u64, 1, 127, 128, 300, 1 << 20, u32::MAX as u64, u64::MAX];
        let mut buf = Vec::new();
        for &v in &vals {
            write_varint(&mut buf, v);
        }
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(read_varint(&buf, &mut pos), v);
        }
        assert_eq!(pos, buf.len());
        let mut p2 = 0;
        skip_varints(&buf, &mut p2, 3);
        assert_eq!(read_varint(&buf, &mut p2), 128);
    }

    #[test]
    fn pack_roundtrip() {
        for n in [1usize, 5, 127, 128] {
            for maxv in [0u32, 1, 7, 1000, u32::MAX] {
                let vals: Vec<u32> = (0..n as u32)
                    .map(|i| {
                        if maxv == 0 {
                            0
                        } else {
                            ((i.wrapping_mul(2654435761)) % maxv.max(1)) | (i % 2)
                        }
                    })
                    .map(|v| v.min(maxv))
                    .collect();
                let mut buf = vec![0xAA];
                pack(&mut buf, &vals);
                buf.push(0x55);
                let mut pos = 1;
                let mut out = vec![0u32; n];
                unpack(&buf, &mut pos, &mut out);
                assert_eq!(out, vals);
                assert_eq!(buf[pos], 0x55);
            }
        }
    }
}
