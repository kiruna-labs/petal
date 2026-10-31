//! The remote-window owner palette, shared by the macOS and Windows
//! compositors so both pick a sharer's border the same way (#322).
//!
//! Keep `OWNER_COLOR_PALETTE_HEX` in lockstep with the TS PALETTE and hex map
//! in apps/desktop/src/lib/data/identityColor.ts.

pub(crate) const OWNER_COLOR_PALETTE_HEX: [&str; 6] = [
    "#f06cc9", // plum
    "#6e8bff", // blue
    "#7ff0a3", // green
    "#e8b84b", // amber
    "#d6b8f0", // lilac
    "#8fa6b8", // slate
];

/// A palette index the sharer published, if it names a palette entry.
pub(crate) fn valid_palette_index(palette_index: Option<u8>) -> Option<u8> {
    palette_index.filter(|index| (*index as usize) < OWNER_COLOR_PALETTE_HEX.len())
}

/// Deterministic pick matching the frontend's `colorForIdentity`: fold UTF-16
/// code units with `h = h * 31 + unit`, then mod the palette length.
pub(crate) fn owner_color_hex(owner: &str) -> &'static str {
    let hash = owner.encode_utf16().fold(0u32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(unit as u32)
    });
    OWNER_COLOR_PALETTE_HEX[(hash as usize) % OWNER_COLOR_PALETTE_HEX.len()]
}

/// The remote-window border must match the header tint. The header uses the
/// sharer's chosen palette index when there is one, otherwise it keys on the
/// owner IDENTITY, falling back to the display name -- `colorForIdentity(
/// ownerIdentity || ownerName)` in surface/+page.svelte. Mirror it here.
pub(crate) fn owner_border_color_hex(
    owner_identity: &str,
    owner_display_name: &str,
    palette_index: Option<u8>,
) -> &'static str {
    if let Some(index) = valid_palette_index(palette_index) {
        return OWNER_COLOR_PALETTE_HEX[index as usize];
    }
    let key = if owner_identity.trim().is_empty() {
        owner_display_name
    } else {
        owner_identity
    };
    owner_color_hex(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chosen_palette_index_wins_over_the_identity_hash() {
        let hashed = owner_color_hex("bob-identity");
        let hashed_index = OWNER_COLOR_PALETTE_HEX.iter().position(|hex| *hex == hashed).unwrap();
        let chosen = ((hashed_index + 1) % OWNER_COLOR_PALETTE_HEX.len()) as u8;
        assert_eq!(
            owner_border_color_hex("bob-identity", "Bob", Some(chosen)),
            OWNER_COLOR_PALETTE_HEX[chosen as usize]
        );
        assert_ne!(owner_border_color_hex("bob-identity", "Bob", Some(chosen)), hashed);
    }

    #[test]
    fn without_a_valid_index_the_border_keys_on_identity_then_name() {
        assert_eq!(owner_border_color_hex("bob-identity", "Bob", None), owner_color_hex("bob-identity"));
        assert_eq!(owner_border_color_hex("bob-identity", "Bob", Some(6)), owner_color_hex("bob-identity"));
        assert_eq!(owner_border_color_hex("  ", "Bob", None), owner_color_hex("Bob"));
        assert_eq!(valid_palette_index(Some(5)), Some(5));
        assert_eq!(valid_palette_index(Some(6)), None);
    }

    #[test]
    fn the_identity_hash_matches_the_frontend() {
        // Same vector as compositor.rs's `colorForIdentity` parity test.
        assert_eq!(owner_color_hex("Someone"), "#d6b8f0");
    }
}
