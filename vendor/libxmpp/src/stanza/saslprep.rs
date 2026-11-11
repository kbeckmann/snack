//! Stored-password SASLprep with the frozen Unicode 3.2 bidirectional rules.
//!
//! Reuse stringprep's RFC mapping/prohibition tables and the maintained NFKC
//! implementation, while avoiding its current-Unicode bidi classifications.

use std::borrow::Cow;
use stringprep::tables;
use unicode_normalization::UnicodeNormalization;

mod bidi;

pub(super) fn password(input: &str) -> Result<Cow<'_, str>, ()> {
    if input.is_ascii() && !input.chars().any(tables::ascii_control_character) {
        return Ok(Cow::Borrowed(input));
    }
    // Modern NFKC can map a character assigned after 3.2 to older text.
    if input.chars().any(tables::unassigned_code_point) { return Err(()); }
    let input = unicode32_normalization_input(input);
    let normalized: String = input.chars()
        .map(|ch| if tables::non_ascii_space_character(ch) { ' ' } else { ch })
        .filter(|ch| !tables::commonly_mapped_to_nothing(*ch))
        .nfkc().collect();
    if normalized.chars().any(|ch|
        tables::non_ascii_space_character(ch) || tables::ascii_control_character(ch)
        || tables::non_ascii_control_character(ch) || tables::private_use(ch)
        || tables::non_character_code_point(ch) || tables::surrogate_code(ch)
        || tables::inappropriate_for_plain_text(ch) || tables::inappropriate_for_canonical_representation(ch)
        || tables::change_display_properties_or_deprecated(ch) || tables::tagging_character(ch)
        || tables::unassigned_code_point(ch)) { return Err(()); }
    if normalized.chars().any(|ch| bidi_property(ch) == 2)
        && (normalized.chars().any(|ch| bidi_property(ch) == 1)
            || !normalized.chars().next().is_some_and(|ch| bidi_property(ch) == 2)
            || !normalized.chars().next_back().is_some_and(|ch| bidi_property(ch) == 2)) {
        return Err(());
    }
    Ok(Cow::Owned(normalized))
}

fn bidi_property(ch: char) -> u8 {
    let contains = |ranges: &[(u32, u32)]| ranges.binary_search_by(|&(lo, hi)| {
        if hi < ch as u32 { std::cmp::Ordering::Less }
        else if lo > ch as u32 { std::cmp::Ordering::Greater }
        else { std::cmp::Ordering::Equal }
    }).is_ok();
    if contains(bidi::RANDAL) { 2 } else if contains(bidi::LCAT) { 1 } else { 0 }
}

fn unicode32_normalization_input(password: &str) -> Cow<'_, str> {
    // Restore the five corrections newer than 3.2 before dependency NFKC.
    // https://www.unicode.org/Public/UCD/latest/ucd/NormalizationCorrections.txt
    // F951's correction is already part of 3.2 and must remain in effect.
    let original = |ch| match ch {
        '\u{2f868}' => '\u{2136a}',
        '\u{2f874}' => '\u{5f33}',
        '\u{2f91f}' => '\u{43ab}',
        '\u{2f95f}' => '\u{7aae}',
        '\u{2f9bf}' => '\u{4d57}',
        ch => ch,
    };
    if password.chars().all(|ch| original(ch) == ch) { Cow::Borrowed(password) }
    else { Cow::Owned(password.chars().map(original).collect()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn frozen_bidi_properties_match_independent_unicode32_for_every_scalar() {
        // Python's frozen RFC3454 stringprep.in_table_d1/in_table_d2 predicates,
        // encoded as 2/1/0 for each Unicode scalar in ascending order.
        let classes: Vec<u8> = (0..=0x10FFFF).filter_map(char::from_u32).map(bidi_property).collect();
        let digest: String = Sha256::digest(&classes).iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(digest,
            "c4d2b34e93a1a693245b05a67648837694f0d68ab41120bddad98b17a76c5ae3");
    }

    #[test]
    fn single_scalar_preparation_matches_independent_unicode32_reference() {
        // Python Unicode3.2 NFKC and RFC3454 tables, with the existing
        // space-mapping-before-removal order. Each result is encoded as a
        // big-endian u32 byte length and UTF8, or FFFFFFFF for rejection.
        let mut fingerprint = Sha256::new();
        let mut accepted = 0;
        for ch in (0..=0x10FFFF).filter_map(char::from_u32) {
            let input = ch.to_string();
            match password(&input) {
                Ok(text) => {
                    fingerprint.update((text.len() as u32).to_be_bytes());
                    fingerprint.update(text.as_bytes());
                    accepted += 1;
                }
                Err(()) => fingerprint.update(u32::MAX.to_be_bytes()),
            }
        }
        let digest: String = fingerprint.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(accepted, 94_963);
        assert_eq!(digest, "c235d6411cfb25deccfb7d94b542a39b414b41c338317dd3fad270e2a9c058ed");
    }

    #[test]
    fn frozen_bidirectional_rules_preserve_allowed_neutral_characters() {
        for middle in (0x2800..=0x28FF).filter_map(char::from_u32)
            .chain(['\u{0cbf}', '\u{0cc6}', '\u{1734}', '\u{2132}', '\u{302e}', '\u{302f}', '1']) {
            let value = format!("\u{0627}{middle}\u{0628}");
            assert_eq!(password(&value).unwrap(), value, "{middle:?}");
        }
        for value in ["\u{0627}1", "1\u{0627}", "\u{0627}a\u{0628}", "a\u{0627}", "\u{0627}a"] {
            assert!(password(value).is_err(), "{value:?}");
        }
    }
}
