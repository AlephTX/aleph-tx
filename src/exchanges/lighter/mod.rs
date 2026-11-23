pub mod error;
pub mod ffi;
pub mod risk;
pub mod trading;

pub(crate) fn valid_market_id(id: i64) -> bool {
    // v1.0.12 reserves 255 as the null market index.
    (0..(1 << 48)).contains(&id) && id != 255
}

/// Validates and copies a secret string into a pre-sized zeroized NUL-terminated buffer.
/// Rejects embedded NUL bytes on the borrowed slice first so `CString::new` never
/// leaves an unzeroized `Vec<u8>` inside a dropped `NulError` or grows on push.
#[cfg_attr(
    not(all(target_os = "linux", target_arch = "x86_64")),
    allow(dead_code)
)]
pub(crate) fn nul_terminated_secret_bytes(
    secret: &str,
) -> Result<k256::elliptic_curve::zeroize::Zeroizing<Vec<u8>>, &'static str> {
    if secret.as_bytes().contains(&0) {
        return Err("Invalid Lighter private key encoding");
    }
    let cap = secret
        .len()
        .checked_add(1)
        .ok_or("Invalid Lighter private key encoding")?;
    let mut buf = k256::elliptic_curve::zeroize::Zeroizing::new(Vec::with_capacity(cap));
    buf.extend_from_slice(secret.as_bytes());
    buf.push(0);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::nul_terminated_secret_bytes;

    #[test]
    fn nul_terminated_secret_bytes_rejects_embedded_nul_and_avoids_reallocation() {
        assert!(nul_terminated_secret_bytes("abc\0def").is_err());
        let ok = nul_terminated_secret_bytes("deadbeef").unwrap();
        assert_eq!(ok.as_slice(), b"deadbeef\0");
        assert_eq!(ok.capacity(), "deadbeef".len() + 1);
    }
}
