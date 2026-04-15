/// COBS (Consistent Overhead Byte Stuffing) encoder/decoder.
/// Delimiter: 0x00. Encoded data never contains 0x00.
pub fn encode(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len() + data.len() / 254 + 2);
    let mut code_index = 0;
    let mut code: u8 = 1;

    output.push(0); // placeholder for first code byte

    for &byte in data {
        if byte == 0 {
            output[code_index] = code;
            code = 1;
            code_index = output.len();
            output.push(0); // placeholder
        } else {
            output.push(byte);
            code += 1;
            if code == 0xFF {
                output[code_index] = code;
                code = 1;
                code_index = output.len();
                output.push(0); // placeholder
            }
        }
    }

    output[code_index] = code;
    output.push(0); // delimiter
    output
}

pub fn decode(data: &[u8]) -> Result<Vec<u8>, DecodeError> {
    // Strip trailing delimiter if present
    let data = if data.last() == Some(&0) {
        &data[..data.len() - 1]
    } else {
        data
    };

    if data.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        let code = data[i] as usize;
        if code == 0 {
            return Err(DecodeError::UnexpectedZero(i));
        }
        i += 1;

        for _ in 1..code {
            if i >= data.len() {
                return Err(DecodeError::Truncated);
            }
            if data[i] == 0 {
                return Err(DecodeError::UnexpectedZero(i));
            }
            output.push(data[i]);
            i += 1;
        }

        if code < 0xFF && i < data.len() {
            output.push(0);
        }
    }

    Ok(output)
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum DecodeError {
    #[error("unexpected zero byte at position {0}")]
    UnexpectedZero(usize),
    #[error("truncated data")]
    Truncated,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let encoded = encode(b"");
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, b"");
    }

    #[test]
    fn roundtrip_no_zeros() {
        let data = b"hello world";
        let decoded = decode(&encode(data)).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_with_zeros() {
        let data = &[0x00, 0x01, 0x00, 0x02, 0x00];
        let decoded = decode(&encode(data)).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_all_zeros() {
        let data = &[0u8; 10];
        let decoded = decode(&encode(data)).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_single_zero() {
        let data = &[0u8];
        let decoded = decode(&encode(data)).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_254_bytes() {
        let data: Vec<u8> = (1..=254).collect();
        let decoded = decode(&encode(&data)).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_255_non_zero_bytes() {
        let data: Vec<u8> = (0..255).map(|i| (i % 254) as u8 + 1).collect();
        let decoded = decode(&encode(&data)).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn encoded_has_no_internal_zeros() {
        let data = &[0, 1, 0, 2, 0, 3, 0];
        let encoded = encode(data);
        // Only the last byte should be zero (delimiter)
        for &b in &encoded[..encoded.len() - 1] {
            assert_ne!(b, 0, "encoded data contains zero before delimiter");
        }
        assert_eq!(*encoded.last().unwrap(), 0);
    }

    #[test]
    fn decode_truncated() {
        // Code byte says 5 more bytes follow, but only 2 available
        let bad = &[5, 1, 2];
        assert_eq!(decode(bad), Err(DecodeError::Truncated));
    }

    #[test]
    fn roundtrip_large_payload() {
        let data: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
        let decoded = decode(&encode(&data)).unwrap();
        assert_eq!(decoded, data);
    }
}
