//! A PNG encoder, in about a hundred lines of std.
//!
//! Written rather than pulled in because of what it is actually for:
//! `--shot`, which renders a panel offscreen so its *appearance* can be
//! looked at without a window.  That is a development tool, and paying
//! a compression dependency — plus its transitive tree, plus its
//! licence in `deny.toml` — for a file nobody transmits would be a bad
//! trade.
//!
//! The trick that makes it short: DEFLATE has a **stored** block type,
//! and zlib is happy to carry stored blocks.  So the "compressor" here
//! copies bytes and writes the right lengths around them.  The output
//! is a few percent larger than the input; every PNG decoder reads it,
//! including Preview and the browser.
//!
//! What is *not* skipped is the checksums.  A PNG with a wrong CRC or
//! a wrong Adler-32 is a file that opens in one viewer and not the
//! next, which is a worse failure than no file at all — the tool would
//! be reporting "here is the panel" while handing over something
//! unopenable.

/// CRC-32 as PNG specifies it (IEEE, reflected, `0xEDB88320`).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Adler-32, the checksum zlib puts at the end of its stream.
fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let mut crc_input = Vec::with_capacity(4 + body.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(body);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Wrap `raw` in a zlib stream made of stored (uncompressed) DEFLATE
/// blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    // 0x78 0x01 = deflate, 32 K window, no preset dict, fastest —
    // and (0x78 << 8 | 0x01) % 31 == 0, which is the header check.
    let mut out = vec![0x78, 0x01];
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    for (i, block) in raw.chunks(0xFFFF).enumerate() {
        let last = (i + 1) * 0xFFFF >= raw.len();
        out.push(if last { 1 } else { 0 });
        let n = block.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Encode 8-bit RGBA, top-left origin, `width * 4` bytes per row.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let want = width as usize * height as usize * 4;
    if rgba.len() < want {
        return Err(format!("need {want} bytes for {width}x{height}, got {}", rgba.len()));
    }
    // Filter byte per row.  Every row uses filter 0 (None): the point
    // of this encoder is to be obviously correct, and predictors only
    // pay off next to a real compressor.
    let mut raw = Vec::with_capacity(height as usize * (1 + width as usize * 4));
    for y in 0..height as usize {
        raw.push(0);
        let row = y * width as usize * 4;
        raw.extend_from_slice(&rgba[row..row + width as usize * 4]);
    }

    let mut out = Vec::with_capacity(raw.len() + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, truecolour+alpha
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// Encode what Metal hands back — BGRA, which is the same bytes in a
/// different order.  Done here rather than at the call site because
/// getting it wrong produces a picture that looks *plausible* (blue
/// and red swapped) rather than broken, and a screenshot tool whose
/// colours are subtly wrong is worse than none.
pub fn encode_bgra(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    let want = width as usize * height as usize * 4;
    if bgra.len() < want {
        return Err(format!("need {want} bytes for {width}x{height}, got {}", bgra.len()));
    }
    let mut rgba = vec![0u8; want];
    for i in 0..want / 4 {
        rgba[i * 4] = bgra[i * 4 + 2];
        rgba[i * 4 + 1] = bgra[i * 4 + 1];
        rgba[i * 4 + 2] = bgra[i * 4];
        rgba[i * 4 + 3] = bgra[i * 4 + 3];
    }
    encode_rgba(width, height, &rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two checksums are the whole reason this file is longer than
    /// twenty lines, so they are pinned against published vectors.
    #[test]
    fn the_checksums_match_the_published_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    /// Structure: signature, then IHDR / IDAT / IEND in order, each
    /// with a length the reader can walk by and a CRC over its own
    /// bytes.  Walking the chunk chain is exactly what a decoder does,
    /// so a file that survives this walk is a file that opens.
    #[test]
    fn the_file_walks_as_a_decoder_would_walk_it() {
        let (w, h) = (7u32, 5u32);
        let px = vec![0xABu8; (w * h * 4) as usize];
        let png = encode_rgba(w, h, &px).expect("encode");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

        let mut i = 8;
        let mut kinds: Vec<String> = Vec::new();
        while i + 8 <= png.len() {
            let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let kind = &png[i + 4..i + 8];
            let body = &png[i + 8..i + 8 + len];
            let want = u32::from_be_bytes(
                png[i + 8 + len..i + 12 + len].try_into().unwrap(),
            );
            let mut crc_input = kind.to_vec();
            crc_input.extend_from_slice(body);
            assert_eq!(crc32(&crc_input), want, "CRC on {:?}", String::from_utf8_lossy(kind));
            kinds.push(String::from_utf8_lossy(kind).into_owned());
            i += 12 + len;
        }
        assert_eq!(i, png.len(), "trailing bytes after the last chunk");
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
    }

    /// The stored-block framing has to survive crossing the 64 K
    /// boundary — one block per 65535 bytes, only the last one flagged
    /// final, and `nlen` the exact complement of `len`.
    #[test]
    fn stored_blocks_frame_correctly_across_the_64k_boundary() {
        for n in [0usize, 1, 0xFFFF - 1, 0xFFFF, 0xFFFF + 1, 3 * 0xFFFF] {
            let raw = vec![0x5Au8; n];
            let z = zlib_stored(&raw);
            assert_eq!(&z[..2], &[0x78, 0x01]);
            assert_eq!(
                ((z[0] as u16) << 8 | z[1] as u16) % 31,
                0,
                "n={n}: zlib header check"
            );
            // Walk the blocks and rebuild the payload.
            let mut i = 2;
            let mut back = Vec::new();
            loop {
                let final_block = z[i] & 1 == 1;
                let len = u16::from_le_bytes([z[i + 1], z[i + 2]]);
                let nlen = u16::from_le_bytes([z[i + 3], z[i + 4]]);
                assert_eq!(nlen, !len, "n={n}: nlen must complement len");
                back.extend_from_slice(&z[i + 5..i + 5 + len as usize]);
                i += 5 + len as usize;
                if final_block {
                    break;
                }
            }
            assert_eq!(back, raw, "n={n}: payload round-trip");
            assert_eq!(
                u32::from_be_bytes(z[i..i + 4].try_into().unwrap()),
                adler32(&raw),
                "n={n}: adler"
            );
            assert_eq!(i + 4, z.len(), "n={n}: trailing bytes");
        }
    }

    /// BGRA in, RGBA out — swapped, not reordered wholesale, and alpha
    /// left where it is.
    #[test]
    fn bgra_becomes_rgba_without_disturbing_alpha() {
        let bgra = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let png = encode_bgra(2, 1, &bgra).expect("encode");
        // Pull the raw scanline back out of the stored block.
        let idat = {
            let mut i = 8;
            loop {
                let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
                if &png[i + 4..i + 8] == b"IDAT" {
                    break png[i + 8..i + 8 + len].to_vec();
                }
                i += 12 + len;
            }
        };
        let row = &idat[2 + 5..2 + 5 + 9];
        assert_eq!(row[0], 0, "filter byte");
        assert_eq!(&row[1..5], &[30, 20, 10, 40], "first pixel");
        assert_eq!(&row[5..9], &[70, 60, 50, 80], "second pixel");
    }
}
