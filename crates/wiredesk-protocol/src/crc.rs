use crc::{Crc, CRC_16_IBM_3740};

const CRC16: Crc<u16> = Crc::<u16>::new(&CRC_16_IBM_3740);

pub fn compute(data: &[u8]) -> u16 {
    CRC16.checksum(data)
}

pub fn verify(data: &[u8], expected: u16) -> bool {
    compute(data) == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_deterministic() {
        let data = b"WireDesk";
        assert_eq!(compute(data), compute(data));
    }

    #[test]
    fn crc_differs_for_different_data() {
        assert_ne!(compute(b"hello"), compute(b"world"));
    }

    #[test]
    fn crc_empty() {
        let _ = compute(b"");
    }

    #[test]
    fn verify_correct() {
        let data = b"test data";
        let checksum = compute(data);
        assert!(verify(data, checksum));
    }

    #[test]
    fn verify_incorrect() {
        assert!(!verify(b"test", 0x0000));
    }
}
