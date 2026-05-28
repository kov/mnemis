//! Decoder for IMAP modified UTF-7 (RFC 3501 §5.1.3) — the encoding IMAP
//! uses to put non-ASCII characters into mailbox names. Servers return
//! things like `Ita&APo-` over the wire; the user expects to see `Itaú`.
//!
//! The decoder is intentionally **infallible**: a malformed shift sequence
//! is returned verbatim instead of bubbling an error, because the worst
//! case ("something looked encoded but wasn't") is a cosmetic glitch in a
//! mailbox name, not data loss. We'd rather show the raw bytes than refuse
//! to render a mailbox.
//!
//! Decoding plain ASCII (`"INBOX"`, `"Archive"`, an already-decoded
//! `"Itaú"`) is a no-op, so this is safe to apply more than once or to
//! values that were never encoded.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;

/// Decode an IMAP-modified-UTF-7 mailbox name into UTF-8.
pub fn decode_mailbox_name(input: &str) -> String {
    // Walk char-by-char (not byte-by-byte) so that already-UTF-8 input —
    // e.g. an already-decoded `Itaú` we're re-reading from the DB — passes
    // through cleanly instead of being chopped into Latin-1 codepoints.
    // Encoded runs are pure ASCII so they still fit in single `char`s.
    let mut out = String::with_capacity(input.len());
    let mut iter = input.char_indices().peekable();

    while let Some((i, c)) = iter.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        // `&-` is the literal-ampersand escape.
        if matches!(iter.peek(), Some((_, '-'))) {
            out.push('&');
            iter.next();
            continue;
        }
        // Find the terminating `-`. Per the spec it MUST be present in a
        // well-formed name; if it isn't, fall back to copying the `&` and
        // moving on rather than swallowing the rest of the string.
        let after = i + '&'.len_utf8();
        let Some(end_rel) = input[after..].find('-') else {
            out.push('&');
            continue;
        };
        let end = after + end_rel;
        let encoded = &input[after..end];
        match decode_shift(encoded.as_bytes()) {
            Some(decoded) => out.push_str(&decoded),
            // Pass the original sequence through unchanged so the user can
            // at least see what the server sent us instead of a mystery
            // empty mailbox.
            None => {
                out.push('&');
                out.push_str(encoded);
                out.push('-');
            }
        }
        // Advance the iterator past the encoded run + terminating `-`.
        while let Some(&(j, _)) = iter.peek() {
            if j > end {
                break;
            }
            iter.next();
        }
    }
    out
}

/// Decode the contents of a single `&…-` shift. Returns `None` on any
/// malformed input — base64 decode failure, non-paired surrogates, odd
/// byte count — so the caller can fall back to verbatim passthrough.
fn decode_shift(encoded: &[u8]) -> Option<String> {
    // Modified base64 uses `,` for the 63rd alphabet character; standard
    // base64 uses `/`. Substitute back before decoding. STANDARD_NO_PAD
    // matches the spec, which omits `=` padding.
    let mut alphabet: Vec<u8> = Vec::with_capacity(encoded.len());
    for &c in encoded {
        alphabet.push(if c == b',' { b'/' } else { c });
    }
    let bytes = STANDARD_NO_PAD.decode(&alphabet).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }

    // Interpret the bytes as UTF-16BE. Pair surrogates manually since
    // `char::from_u32` won't accept lone halves.
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();

    let mut out = String::new();
    let mut i = 0;
    while i < units.len() {
        let u = units[i];
        if (0xD800..0xDC00).contains(&u) {
            // High surrogate — needs a low surrogate to be valid.
            if i + 1 >= units.len() {
                return None;
            }
            let low = units[i + 1];
            if !(0xDC00..0xE000).contains(&low) {
                return None;
            }
            let cp = 0x10000 + (((u as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
            out.push(char::from_u32(cp)?);
            i += 2;
        } else if (0xDC00..0xE000).contains(&u) {
            // Unpaired low surrogate.
            return None;
        } else {
            out.push(char::from_u32(u as u32)?);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through() {
        assert_eq!(decode_mailbox_name("INBOX"), "INBOX");
        assert_eq!(decode_mailbox_name(""), "");
        assert_eq!(decode_mailbox_name("INBOX/Archive"), "INBOX/Archive");
    }

    #[test]
    fn decodes_latin_accent() {
        // The motivating case: `Ita&APo-` is what the server sends for
        // `Itaú` (ú = U+00FA = 0x00FA → base64 "APo").
        assert_eq!(decode_mailbox_name("Ita&APo-"), "Itaú");
    }

    #[test]
    fn literal_ampersand_is_escaped() {
        assert_eq!(decode_mailbox_name("AT&-T"), "AT&T");
    }

    #[test]
    fn handles_multiple_shifts_and_hierarchy() {
        // Hierarchy delimiter `/` lives outside the shift, gets passed
        // through, and the second shift decodes independently.
        assert_eq!(
            decode_mailbox_name("INBOX/Ita&APo-/Sub&APo-"),
            "INBOX/Itaú/Subú"
        );
    }

    #[test]
    fn rfc3501_cyrillic_example() {
        // From RFC 3501 §5.1.3: "&BBoEPgRABD8EQwRB-" → "Корпус".
        assert_eq!(decode_mailbox_name("&BBoEPgRABD8EQwRB-"), "Корпус");
    }

    #[test]
    fn already_decoded_is_idempotent() {
        // Important: applying the decoder twice (e.g. once at ingest, once
        // when reading back already-decoded rows) must not corrupt
        // anything.
        let once = decode_mailbox_name("Ita&APo-");
        let twice = decode_mailbox_name(&once);
        assert_eq!(twice, "Itaú");
    }

    #[test]
    fn malformed_shift_is_passed_through() {
        // Unterminated `&` shift and an obviously-invalid base64 payload
        // should not panic; the verbatim text is the least-surprising
        // fallback.
        assert_eq!(decode_mailbox_name("foo&"), "foo&");
        assert_eq!(decode_mailbox_name("foo&!!!-bar"), "foo&!!!-bar");
    }
}
