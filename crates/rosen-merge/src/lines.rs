//! Line splitting and interning.
//!
//! Lines keep their terminator, so `"a\n"` and a final `"a"` with no newline
//! are different lines. Joining the lines of a file gives back its exact
//! bytes, which is what lets a clean merge reproduce a side byte for byte.

use std::borrow::Cow;
use std::collections::HashMap;

/// Splits `text` into lines, each ending with `\n` except possibly the last.
pub fn split_lines(text: &[u8]) -> Vec<&[u8]> {
    text.split_inclusive(|&byte| byte == b'\n').collect()
}

/// Maps each distinct line to a small integer, so the diff compares `u32`s
/// instead of byte strings. One interner is shared by all three sides of a
/// merge, so equal lines get equal ids across files.
#[derive(Default)]
pub struct LineInterner<'a> {
    ids: HashMap<&'a [u8], u32>,
}

impl<'a> LineInterner<'a> {
    pub fn intern_all(&mut self, lines: &[&'a [u8]]) -> Vec<u32> {
        lines.iter().map(|line| self.intern(line)).collect()
    }

    fn intern(&mut self, line: &'a [u8]) -> u32 {
        let next_id = u32::try_from(self.ids.len()).expect("more than u32::MAX distinct lines");
        *self.ids.entry(line).or_insert(next_id)
    }
}

/// Uses the base's first terminated line as the file's newline convention.
/// Bare CR bytes and missing final newlines are content and stay untouched.
/// Without a terminated base line there is no convention to impose.
pub fn normalize_line_endings<'a>(text: &'a [u8], base: &[u8]) -> Cow<'a, [u8]> {
    let Some(newline_at) = base.iter().position(|&byte| byte == b'\n') else {
        return Cow::Borrowed(text);
    };
    let crlf = newline_at > 0 && base[newline_at - 1] == b'\r';
    let needs_normalization = text
        .iter()
        .enumerate()
        .any(|(at, &byte)| byte == b'\n' && (at > 0 && text[at - 1] == b'\r') != crlf);
    if !needs_normalization {
        return Cow::Borrowed(text);
    }
    let mut normalized = Vec::with_capacity(text.len());
    for line in text.split_inclusive(|&byte| byte == b'\n') {
        if let Some(body) = line.strip_suffix(b"\n") {
            normalized.extend_from_slice(body.strip_suffix(b"\r").unwrap_or(body));
            normalized.extend_from_slice(if crlf { b"\r\n" } else { b"\n" });
        } else {
            normalized.extend_from_slice(line);
        }
    }
    Cow::Owned(normalized)
}
