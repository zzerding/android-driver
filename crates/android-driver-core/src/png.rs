pub const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;

pub mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let first = chunk[0] as u32;
            let second = chunk.get(1).copied().unwrap_or(0) as u32;
            let third = chunk.get(2).copied().unwrap_or(0) as u32;
            let value = (first << 16) | (second << 8) | third;
            output.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
            output.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                output.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
            } else {
                output.push('=');
            }
            if chunk.len() > 2 {
                output.push(ALPHABET[(value & 0x3f) as usize] as char);
            } else {
                output.push('=');
            }
        }
        output
    }

    pub fn decode(input: &str) -> std::result::Result<Vec<u8>, String> {
        if !input.len().is_multiple_of(4) {
            return Err("base64 length is not a multiple of four".to_owned());
        }
        let bytes = input.as_bytes();
        let mut output = Vec::with_capacity(input.len() / 4 * 3);
        let mut index = 0;
        while index < bytes.len() {
            let chunk = &bytes[index..index + 4];
            let last_chunk = index + 4 == bytes.len();
            let mut values = [0u32; 4];
            let mut padding = 0usize;
            for (slot, byte) in chunk.iter().enumerate() {
                if *byte == b'=' {
                    if !last_chunk || slot < 2 || (slot == 2 && chunk[3] != b'=') {
                        return Err("base64 padding is not at the end".to_owned());
                    }
                    padding += 1;
                    values[slot] = 0;
                } else {
                    if padding > 0 {
                        return Err("base64 padding is not at the end".to_owned());
                    }
                    values[slot] = decode_byte(*byte)
                        .ok_or_else(|| "base64 contains an invalid character".to_owned())?;
                }
            }
            if padding > 2 {
                return Err("base64 has too much padding".to_owned());
            }
            let value = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
            output.push(((value >> 16) & 0xff) as u8);
            if padding < 2 {
                output.push(((value >> 8) & 0xff) as u8);
            }
            if padding < 1 {
                output.push((value & 0xff) as u8);
            }
            index += 4;
        }
        Ok(output)
    }

    fn decode_byte(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
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
