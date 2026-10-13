use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Hash several fields without ambiguity: `("ab", "c")` and `("a", "bc")`
/// must not collide, so each field is length-prefixed.
pub fn sha256_hex_fields(fields: &[&[u8]]) -> String {
    let mut buf = Vec::new();
    for field in fields {
        buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
        buf.extend_from_slice(field);
    }
    sha256_hex(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fields_are_unambiguous() {
        assert_ne!(
            sha256_hex_fields(&[b"ab", b"c"]),
            sha256_hex_fields(&[b"a", b"bc"])
        );
    }
}
