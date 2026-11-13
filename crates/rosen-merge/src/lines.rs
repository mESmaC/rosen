//! Line splitting and interning.
//!
//! Lines keep their terminator, so `"a\n"` and a final `"a"` with no newline
//! are different lines. Joining the lines of a file gives back its exact
//! bytes, which is what lets a clean merge reproduce a side byte for byte.

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
