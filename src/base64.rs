//! Base64 decoding, standard and URL-safe, with or without padding.

#[derive(Clone, Copy)]
pub enum Alphabet {
    /// `+` and `/`.
    Standard,
    /// `-` and `_`, as in JWTs.
    UrlSafe,
}

/// Decodes `input`. Padding is optional, but if present it must be right.
/// Bits left over after the last byte must be zero, so each decoding has
/// one encoding.
pub fn decode(input: &str, alphabet: Alphabet) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let data = match bytes.iter().position(|b| *b == b'=') {
        Some(at) => {
            let padding = &bytes[at..];
            if !bytes.len().is_multiple_of(4)
                || padding.len() > 2
                || padding.iter().any(|b| *b != b'=')
            {
                return None;
            }
            &bytes[..at]
        }
        None => bytes,
    };
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0;
    for &byte in data {
        buffer = (buffer << 6) | u32::from(sextet(byte, alphabet)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    if buffer & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

fn sextet(byte: u8, alphabet: Alphabet) -> Option<u8> {
    match (byte, alphabet) {
        (b'A'..=b'Z', _) => Some(byte - b'A'),
        (b'a'..=b'z', _) => Some(byte - b'a' + 26),
        (b'0'..=b'9', _) => Some(byte - b'0' + 52),
        (b'+', Alphabet::Standard) | (b'-', Alphabet::UrlSafe) => Some(62),
        (b'/', Alphabet::Standard) | (b'_', Alphabet::UrlSafe) => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::base64::Engine;
    use ::base64::alphabet;
    use ::base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

    fn engine(alphabet: Alphabet, padding: DecodePaddingMode) -> GeneralPurpose {
        let config = GeneralPurposeConfig::new()
            .with_decode_padding_mode(padding)
            .with_decode_allow_trailing_bits(false);
        match alphabet {
            Alphabet::Standard => GeneralPurpose::new(&alphabet::STANDARD, config),
            Alphabet::UrlSafe => GeneralPurpose::new(&alphabet::URL_SAFE, config),
        }
    }

    /// Padding is optional, but right if present: either the canonical
    /// engine or the unpadded one accepts. The crate's `Indifferent` mode
    /// also takes too little padding, which to11y refuses.
    fn oracle(input: &str, alphabet: Alphabet) -> Option<Vec<u8>> {
        engine(alphabet, DecodePaddingMode::RequireCanonical)
            .decode(input)
            .or_else(|_| engine(alphabet, DecodePaddingMode::RequireNone).decode(input))
            .ok()
    }

    #[test]
    fn rfc_4648_vectors_decode() {
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
            ("Zg", "f"),
            ("Zm8", "fo"),
        ] {
            assert_eq!(
                decode(encoded, Alphabet::Standard).as_deref(),
                Some(plain.as_bytes()),
                "{encoded}"
            );
        }
    }

    #[test]
    fn decoding_matches_the_base64_crate_on_random_input() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let symbols = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/-_=. ";
        for round in 0..100_000 {
            let alphabet = if round % 2 == 0 {
                Alphabet::Standard
            } else {
                Alphabet::UrlSafe
            };
            let input: String = if round % 3 == 0 {
                // Mostly valid: encode random bytes, then maybe damage them.
                let length = (next() % 40) as usize;
                let plain: Vec<u8> = (0..length).map(|_| next() as u8).collect();
                let mut text = engine(alphabet, DecodePaddingMode::RequireCanonical).encode(plain);
                if next() % 2 == 0 {
                    text = text.trim_end_matches('=').to_owned();
                }
                if next() % 4 == 0 && !text.is_empty() {
                    let at = (next() as usize) % text.len();
                    let symbol = symbols[(next() as usize) % symbols.len()] as char;
                    text.replace_range(at..at + 1, &symbol.to_string());
                }
                text
            } else {
                let length = (next() % 12) as usize;
                (0..length)
                    .map(|_| symbols[(next() as usize) % symbols.len()] as char)
                    .collect()
            };
            assert_eq!(
                decode(&input, alphabet),
                oracle(&input, alphabet),
                "{input:?}"
            );
        }
    }
}
