pub const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;

pub mod base64 {
    use ::base64::{Engine as _, engine::general_purpose::STANDARD};

    pub fn encode(input: &[u8]) -> String {
        STANDARD.encode(input)
    }

    pub fn decode(input: &str) -> std::result::Result<Vec<u8>, String> {
        STANDARD.decode(input).map_err(|error| error.to_string())
    }
}

pub fn validate_png(
    bytes: &[u8],
    expected_width: u32,
    expected_height: u32,
) -> std::result::Result<(), String> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() > MAX_PNG_BYTES {
        return Err(format!("PNG is larger than the {MAX_PNG_BYTES} byte limit"));
    }
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err("screenshot is not a PNG".to_owned());
    }

    let mut position = 8;
    let mut chunk_index = 0usize;
    let mut saw_ihdr = false;
    let mut saw_idat = false;
    let mut saw_iend = false;
    while position < bytes.len() {
        if position + 12 > bytes.len() {
            return Err("PNG chunk header is truncated".to_owned());
        }
        let length = u32::from_be_bytes(
            bytes[position..position + 4]
                .try_into()
                .expect("four bytes"),
        ) as usize;
        let chunk_end = position
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .ok_or_else(|| "PNG chunk length overflows".to_owned())?;
        if chunk_end > bytes.len() {
            return Err("PNG chunk data is truncated".to_owned());
        }
        let kind = &bytes[position + 4..position + 8];
        let data = &bytes[position + 8..position + 8 + length];
        let stored_crc = u32::from_be_bytes(
            bytes[position + 8 + length..chunk_end]
                .try_into()
                .expect("four bytes"),
        );
        let mut crc_input = Vec::with_capacity(4 + length);
        crc_input.extend_from_slice(kind);
        crc_input.extend_from_slice(data);
        if crc32(&crc_input) != stored_crc {
            return Err(format!(
                "PNG chunk {} has an invalid checksum",
                String::from_utf8_lossy(kind)
            ));
        }

        match kind {
            b"IHDR" => {
                if chunk_index != 0 || saw_ihdr {
                    return Err("PNG IHDR is not the first and only header".to_owned());
                }
                if length != 13 {
                    return Err("PNG IHDR has the wrong length".to_owned());
                }
                let width = u32::from_be_bytes(data[0..4].try_into().expect("four bytes"));
                let height = u32::from_be_bytes(data[4..8].try_into().expect("four bytes"));
                if width != expected_width || height != expected_height {
                    return Err(format!(
                        "PNG dimensions {width}x{height} do not match the virtual display {expected_width}x{expected_height}"
                    ));
                }
                if width == 0 || height == 0 {
                    return Err("PNG dimensions must be positive".to_owned());
                }
                saw_ihdr = true;
            }
            b"IDAT" => {
                if !saw_ihdr {
                    return Err("PNG IDAT appears before IHDR".to_owned());
                }
                if length > 0 {
                    saw_idat = true;
                }
            }
            b"IEND" => {
                if length != 0 {
                    return Err("PNG IEND must be empty".to_owned());
                }
                saw_iend = true;
                position = chunk_end;
                if position != bytes.len() {
                    return Err("PNG has trailing bytes after IEND".to_owned());
                }
                break;
            }
            _ => {
                if kind[0].is_ascii_uppercase() {
                    return Err(format!(
                        "PNG contains an unknown critical chunk {}",
                        String::from_utf8_lossy(kind)
                    ));
                }
            }
        }
        chunk_index += 1;
        position = chunk_end;
    }

    if !saw_ihdr {
        return Err("PNG is missing IHDR".to_owned());
    }
    if !saw_idat {
        return Err("PNG is missing image data".to_owned());
    }
    if !saw_iend {
        return Err("PNG is missing IEND".to_owned());
    }
    Ok(())
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc_input = Vec::from(*kind);
        crc_input.extend_from_slice(data);
        bytes.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        bytes
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&chunk(b"IHDR", &header));
        bytes.extend_from_slice(&chunk(
            b"IDAT",
            &[0x78, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        ));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        bytes
    }

    #[test]
    fn accepts_a_well_formed_png_with_matching_dimensions() {
        validate_png(&png(360, 640), 360, 640).unwrap();
    }

    #[test]
    fn rejects_dimension_mismatch_and_corruption() {
        assert!(validate_png(&png(360, 640), 1080, 2400).is_err());
        let mut broken = png(360, 640);
        broken[20] ^= 1;
        assert!(validate_png(&broken, 360, 640).is_err());
        assert!(validate_png(b"not a png", 1, 1).is_err());
    }

    #[test]
    fn rejects_invalid_headers_and_oversized_pngs() {
        let mut wrong_header = png(360, 640);
        wrong_header[12..16].copy_from_slice(b"IDAT");
        assert!(validate_png(&wrong_header, 360, 640).is_err());
        assert!(validate_png(&png(0, 640), 0, 640).is_err());
        assert!(validate_png(&png(360, 640)[..20], 360, 640).is_err());
        assert!(validate_png(&vec![0; MAX_PNG_BYTES + 1], 360, 640).is_err());
    }

    #[test]
    fn png_validation_does_not_decode_image_data() {
        let original = png(360, 640);
        let mut bytes = original[..33].to_vec();
        bytes.extend_from_slice(&chunk(b"IDAT", b"not compressed image data"));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        validate_png(&bytes, 360, 640).unwrap();
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64::encode(plain.as_bytes()), encoded);
            assert_eq!(base64::decode(encoded).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn base64_rejects_noncanonical_padding_and_trailing_bits() {
        for encoded in ["Zg", "Zg=", "Zg===", "Zh==", "Zm9=", "Zg==\n", "-w=="] {
            assert!(base64::decode(encoded).is_err(), "accepted {encoded:?}");
        }
    }

    #[test]
    fn base64_round_trips_and_rejects_garbage() {
        for value in [b"".as_slice(), b"a", b"ab", b"abc", b"\x00\xff\x80"] {
            let encoded = base64::encode(value);
            assert_eq!(base64::decode(&encoded).unwrap(), value);
        }
        assert!(base64::decode("abc").is_err());
        assert!(base64::decode("a===").is_err());
        assert!(base64::decode("ab=c").is_err());
        assert!(base64::decode("ab!c").is_err());
    }
}
