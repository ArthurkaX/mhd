//! Char-boundary-safe helpers for the byte-index cursors used by the settings
//! text editors.
//!
//! Edit fields carry their caret as a byte offset into a `String`, but the
//! initial text comes from the TOML config and can hold multi-byte UTF-8. A
//! caret stepped by one raw byte lands inside a character, and the next
//! `remove` / `drain` / `&text[..cursor]` panics. These helpers keep every
//! cursor on a character boundary.

/// Clamp `i` to the nearest char boundary at or before it, and to `s.len()`.
pub fn clamp(s: &str, i: usize) -> usize {
    if i >= s.len() {
        s.len()
    } else {
        s.floor_char_boundary(i)
    }
}

/// Byte offset of the character before `i`, or 0 if already at the start.
pub fn prev(s: &str, i: usize) -> usize {
    let i = clamp(s, i);
    match s[..i].chars().next_back() {
        Some(c) => i - c.len_utf8(),
        None => 0,
    }
}

/// Byte offset of the character after `i`, or `s.len()` if already at the end.
pub fn next(s: &str, i: usize) -> usize {
    let i = clamp(s, i);
    match s[i..].chars().next() {
        Some(c) => i + c.len_utf8(),
        None => s.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: &str = "aé漢b"; // byte layout: a=0, é=1..3, 漢=3..6, b=6

    #[test]
    fn clamp_floors_into_char_and_caps_at_len() {
        assert_eq!(clamp(M, 0), 0);
        assert_eq!(clamp(M, 2), 1); // mid-'é' -> start of 'é'
        assert_eq!(clamp(M, 5), 3); // mid-'漢' -> start of '漢'
        assert_eq!(clamp(M, 99), M.len());
    }

    #[test]
    fn prev_walks_whole_chars_and_stops_at_zero() {
        assert_eq!(prev(M, M.len()), 6);
        assert_eq!(prev(M, 6), 3);
        assert_eq!(prev(M, 3), 1);
        assert_eq!(prev(M, 1), 0);
        assert_eq!(prev(M, 0), 0);
    }

    #[test]
    fn next_walks_whole_chars_and_stops_at_len() {
        assert_eq!(next(M, 0), 1);
        assert_eq!(next(M, 1), 3);
        assert_eq!(next(M, 3), 6);
        assert_eq!(next(M, 6), M.len());
        assert_eq!(next(M, M.len()), M.len());
    }

    #[test]
    fn movement_from_a_mid_char_index_is_still_a_boundary() {
        // A cursor that was already corrupted must not panic and must resolve
        // onto a boundary rather than propagating the bad offset.
        assert_eq!(prev(M, 2), 0);
        assert_eq!(next(M, 2), 3);
        assert_eq!(next(M, 4), 6);
        for i in 0..=M.len() + 3 {
            assert!(M.is_char_boundary(clamp(M, i)));
            assert!(M.is_char_boundary(prev(M, i)));
            assert!(M.is_char_boundary(next(M, i)));
        }
    }

    #[test]
    fn empty_string_is_inert() {
        assert_eq!(clamp("", 5), 0);
        assert_eq!(prev("", 5), 0);
        assert_eq!(next("", 5), 0);
    }
}
