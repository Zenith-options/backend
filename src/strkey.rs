//! Minimal Stellar "strkey" codec — just enough to turn a `G...` address
//! into the raw 32-byte ed25519 public key it encodes, and back, without
//! pulling in the full stellar-sdk crate for one conversion.

use data_encoding::BASE32_NOPAD;

const ED25519_PUBLIC_KEY_VERSION_BYTE: u8 = 6 << 3; // encodes to the 'G' prefix

/// CRC16/XMODEM: poly 0x1021, init 0x0000, no reflection, no xor-out.
/// Stellar strkey uses this exact variant for its trailing checksum.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0x0000;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub fn encode_stellar_public_key(pubkey: &[u8; 32]) -> String {
    let mut payload = Vec::with_capacity(33);
    payload.push(ED25519_PUBLIC_KEY_VERSION_BYTE);
    payload.extend_from_slice(pubkey);

    let checksum = crc16_xmodem(&payload);
    payload.extend_from_slice(&checksum.to_le_bytes());

    BASE32_NOPAD.encode(&payload)
}

pub fn decode_stellar_public_key(address: &str) -> Result<[u8; 32], String> {
    let raw = BASE32_NOPAD
        .decode(address.as_bytes())
        .map_err(|_| "address is not valid base32".to_string())?;

    if raw.len() != 35 {
        return Err(format!("expected a 35-byte strkey payload, got {}", raw.len()));
    }
    if raw[0] != ED25519_PUBLIC_KEY_VERSION_BYTE {
        return Err("address is not an ed25519 public key (wrong version byte)".to_string());
    }

    let expected_checksum = u16::from_le_bytes([raw[33], raw[34]]);
    let actual_checksum = crc16_xmodem(&raw[0..33]);
    if actual_checksum != expected_checksum {
        return Err("address checksum mismatch".to_string());
    }

    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&raw[1..33]);
    Ok(pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_encode_and_decode() {
        let pubkey: [u8; 32] = std::array::from_fn(|i| i as u8);
        let address = encode_stellar_public_key(&pubkey);
        assert!(address.starts_with('G'));
        let decoded = decode_stellar_public_key(&address).unwrap();
        assert_eq!(decoded, pubkey);
    }

    #[test]
    fn rejects_flipped_checksum_byte() {
        let pubkey = [7u8; 32];
        let mut address = encode_stellar_public_key(&pubkey);
        // Flip the last character to corrupt the checksum without changing length.
        let last = address.pop().unwrap();
        address.push(if last == 'A' { 'B' } else { 'A' });
        assert!(decode_stellar_public_key(&address).is_err());
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(decode_stellar_public_key("GAAA").is_err());
    }
}
