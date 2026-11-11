//! Compare stanza recipients with the bound account without rewriting JIDs.

pub(crate) fn matches_account(recipient: &str, bound: &str) -> bool
{
    if crate::xmpp::parse_address(recipient).is_err() || crate::xmpp::parse_address(bound).is_err() { return false; }
    let recipient = recipient.split('/').next().unwrap_or(recipient);
    let bound = bound.split('/').next().unwrap_or(bound);
    if crate::stanza::jid_matches(recipient, bound) { return true; }
    matches!((account_key(recipient), account_key(bound)), (Some(recipient), Some(bound)) if recipient == bound)
}

fn account_key(jid: &str) -> Option<(Option<String>, String)>
{
    let (local, domain) = crate::xmpp::parse_address(jid).ok()?;
    // RFC 6122 nodeprep (case folding and NFKC), as servers use to decide
    // which spellings name the same account.
    let local = if let Some(local) = local {
        let mapped = stringprep::nodeprep(local).ok()?.into_owned();
        if mapped.is_empty() || mapped.len() > crate::MAX_JID_PART_BYTES { return None; }
        Some(mapped)
    } else { None };
    let domain = if let Some(ip) = domain.strip_prefix('[').and_then(|value| value.strip_suffix(']')) {
        format!("[{}]", ip.parse::<std::net::Ipv6Addr>().ok()?)
    } else {
        let domain = domain.strip_suffix('.').unwrap_or(domain);
        if domain.is_empty() { return None; }
        if domain.is_ascii() && !domain.split('.').any(|label| label.len() > 4 && label[..4].eq_ignore_ascii_case("xn--")) {
            // Keep the existing ASCII host syntax accepted by the connection;
            // recipient comparison must not introduce a new DNS admission policy.
            domain.to_ascii_lowercase()
        } else {
            // Compare internationalized domains in their Unicode form, so an
            // A-label (xn--) and its U-label name the same domain.
            let mut labels = Vec::new();
            for label in domain.split('.') {
                if label.is_empty() || label.len() > 63 { return None; }
                let unicode = match label.get(..4) {
                    Some(prefix) if prefix.eq_ignore_ascii_case("xn--") => punycode_decode(&label[4..].to_ascii_lowercase())?,
                    _ => label.to_string(),
                };
                labels.push(stringprep::nameprep(&unicode).ok()?.into_owned());
            }
            labels.join(".")
        }
    };
    Some((local, domain))
}

// RFC 3492 Punycode decoding of one label's encoded part.
fn punycode_decode(input: &str) -> Option<String>
{
    const BASE: u32 = 36; const TMIN: u32 = 1; const TMAX: u32 = 26;
    const SKEW: u32 = 38; const DAMP: u32 = 700;
    fn adapt(mut delta: u32, points: u32, first: bool) -> u32 {
        delta /= if first { DAMP } else { 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > ((BASE - TMIN) * TMAX) / 2 { delta /= BASE - TMIN; k += BASE; }
        k + (BASE - TMIN + 1) * delta / (delta + SKEW)
    }
    let (basic, encoded) = match input.rfind('-') { Some(at) => (&input[..at], &input[at + 1..]), None => ("", input) };
    if !basic.is_ascii() { return None; }
    let mut output: Vec<char> = basic.chars().collect();
    let (mut n, mut i, mut bias) = (128u32, 0u32, 72u32);
    let mut digits = encoded.bytes().peekable();
    while digits.peek().is_some() {
        let (old_i, mut w, mut k) = (i, 1u32, BASE);
        loop {
            let digit = match digits.next()? {
                byte @ b'a'..=b'z' => u32::from(byte - b'a'),
                byte @ b'0'..=b'9' => u32::from(byte - b'0') + 26,
                _ => return None,
            };
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias { TMIN } else if k >= bias + TMAX { TMAX } else { k - bias };
            if digit < t { break; }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let points = output.len() as u32 + 1;
        bias = adapt(i - old_i, points, old_i == 0);
        n = n.checked_add(i / points)?;
        i %= points;
        output.insert(i as usize, char::from_u32(n)?);
        i += 1;
    }
    Some(output.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::matches_account;

    #[test]
    fn recipient_account_mapping_preserves_protocol_equivalence_and_distinctions() {
        // RFC 6122 stringprep: case folding maps ß to ss and final sigma to
        // sigma, NFKC expands ligatures, and nameprep applies to domains.
        for (recipient, bound, same) in [
            ("ME@EXAMPLE/old-resource", "me@example/current", true),
            ("ＭＥ@example", "me@example/current", true),
            ("ME@chat_internal/old", "me@chat_internal/current", true),
            ("me@example.", "me@example/current", true),
            ("e\u{301}va@example", "ÉVA@example/current", true),
            ("ǅ@example", "ǆ@example/current", true),
            ("ΟΣ@example", "ος@example/current", true),
            ("οσ@example", "ος@example/current", true),
            // Stringprep uses Unicode 3.2; later case pairs stay distinct.
            ("\u{16ea0}@example", "\u{16ebb}@example/current", false),
            ("İ@example", "i\u{307}@example/current", true),
            ("İ@example", "i@example/current", false),
            ("ẞ@example", "ß@example/current", false),
            ("SS@example", "ß@example/current", true),
            ("ﬃ@example", "ffi@example/current", true),
            ("me@BÜCHER.example", "ME@xn--bcher-kva.example./current", true),
            ("me@faß.example", "me@xn--fa-hia.example/current", true),
            ("me@faß.example", "me@fass.example/current", true),
            ("ME@[2001:db8::1]/old", "me@[2001:0db8:0:0:0:0:0:1]/current", true),
            ("other@example", "me@example/current", false),
            ("me@elsewhere.example", "me@example/current", false),
            ("example", "me@example/current", false),
            ("me@example/", "me@example/current", false),
            ("me@example..", "me@example/current", false),
            ("me@münchen.example", "me@XN--MNCHEN-3YA.example/current", true),
            ("me@xn--invalid-!.example", "me@example/current", false),
        ] {
            assert_eq!(matches_account(recipient, bound), same, "recipient={recipient:?}, bound={bound:?}");
        }
    }
}
