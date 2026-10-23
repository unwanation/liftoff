//! New tenant ids: ULIDs (the core's `TenantId`), 48 bits of Unix
//! milliseconds and 80 random bits in Crockford base32.

use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford's base32 alphabet.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A new ULID; `None` without randomness.
pub(crate) fn new() -> Option<String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let mut random = [0_u8; 10];
    getrandom::fill(&mut random).ok()?;
    let mut value = u128::from(u64::try_from(millis & 0xFFFF_FFFF_FFFF).unwrap_or(0)) << 80;
    for (index, byte) in random.iter().enumerate() {
        let shift = u32::try_from(index).ok()?.checked_mul(8)?;
        value |= u128::from(*byte) << (72_u32.checked_sub(shift)?);
    }
    let mut text = String::with_capacity(26);
    for position in (0..26_u32).rev() {
        let shift = position.checked_mul(5)?;
        let digit = usize::try_from((value >> shift) & 0x1F).ok()?;
        text.push(char::from(*ALPHABET.get(digit)?));
    }
    Some(text)
}
