//! Two-way line diff: Myers' O(ND) algorithm in linear space.
//!
//! The merge needs only which base lines each side kept, so the diff returns
//! a match map rather than an edit script. Every match pairs equal lines and
//! matches are strictly increasing on both sides, which is what the diff3
//! chunker relies on.

/// Work budget, in snake steps, for one middle-snake search. Past it the
/// range is treated as fully replaced: the merge stays correct and only gets
/// coarser (a wider region, possibly a conflict), instead of taking minutes
/// on two large, unrelated files.
const SNAKE_WORK_BUDGET: usize = 1 << 28;

/// Returns, for each line of `old`, the index of the line of `new` it is kept
/// as, or `None` if the line was deleted.
pub fn match_lines(old: &[u32], new: &[u32]) -> Vec<Option<usize>> {
    let mut matches = vec![None; old.len()];
    diff_range(old, new, 0, 0, &mut matches);
    matches
}

fn diff_range(
    old: &[u32],
    new: &[u32],
    old_at: usize,
    new_at: usize,
    matches: &mut [Option<usize>],
) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    for i in 0..prefix {
        matches[old_at + i] = Some(new_at + i);
    }
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_len, new_len) = (old.len() - suffix, new.len() - suffix);
    for i in 0..suffix {
        matches[old_at + prefix + old_len + i] = Some(new_at + prefix + new_len + i);
    }
    let (old, new) = (&old[..old_len], &new[..new_len]);
    if old.is_empty() || new.is_empty() {
        return;
    }
    // A split at either corner would recurse on the same range forever, so
    // it is treated like no split: the range is a plain replacement.
    let Some((x, y)) = middle_snake(old, new)
        .filter(|&(x, y)| (x, y) != (0, 0) && (x, y) != (old.len(), new.len()))
    else {
        return;
    };
    let (old_at, new_at) = (old_at + prefix, new_at + prefix);
    diff_range(&old[..x], &new[..y], old_at, new_at, matches);
    diff_range(&old[x..], &new[y..], old_at + x, new_at + y, matches);
}

/// Search state for one middle snake: `forward[k]` and `backward[k]` hold the
/// furthest x reached on diagonal k from the start and from the end.
struct SnakeSearch<'a> {
    old: &'a [u32],
    new: &'a [u32],
    offset: isize,
    forward: Vec<isize>,
    backward: Vec<isize>,
    /// Diagonals that ran off the edit graph are skipped from then on.
    trim: [isize; 4],
}

/// Finds a point on an optimal edit path roughly halfway through it, where
/// the forward and backward searches meet. Returns `None` if the two ranges
/// share no line or the work budget runs out.
fn middle_snake(old: &[u32], new: &[u32]) -> Option<(usize, usize)> {
    let max_d = (old.len() + new.len()).div_ceil(2);
    let budget_d = (SNAKE_WORK_BUDGET / (old.len() + new.len())).max(1);
    let mut search = SnakeSearch {
        old,
        new,
        offset: max_d as isize,
        forward: vec![-1; 2 * max_d + 2],
        backward: vec![-1; 2 * max_d + 2],
        trim: [0; 4],
    };
    search.forward[max_d + 1] = 0;
    search.backward[max_d + 1] = 0;
    // With an odd length difference the paths can only meet on a forward
    // step, with an even one only on a backward step.
    let meet_forward = (old.len() as isize - new.len() as isize) % 2 != 0;
    for d in 0..max_d.min(budget_d) as isize {
        if let Some(split) = search.forward_step(d, meet_forward) {
            return Some(split);
        }
        if let Some(split) = search.backward_step(d, !meet_forward) {
            return Some(split);
        }
    }
    None
}

impl SnakeSearch<'_> {
    fn lens(&self) -> (isize, isize) {
        (self.old.len() as isize, self.new.len() as isize)
    }

    fn forward_step(&mut self, d: isize, check_meet: bool) -> Option<(usize, usize)> {
        let (n, m) = self.lens();
        let delta = n - m;
        let mut k = -d + self.trim[0];
        while k <= d - self.trim[1] {
            let at = (self.offset + k) as usize;
            let mut x = if k == -d || (k != d && self.forward[at - 1] < self.forward[at + 1]) {
                self.forward[at + 1]
            } else {
                self.forward[at - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && self.old[x as usize] == self.new[y as usize] {
                x += 1;
                y += 1;
            }
            self.forward[at] = x;
            if x > n {
                self.trim[1] += 2;
            } else if y > m {
                self.trim[0] += 2;
            } else if check_meet {
                let other = self.offset + delta - k;
                if other >= 0 && (other as usize) < self.backward.len() {
                    let reached = self.backward[other as usize];
                    if reached != -1 && x >= n - reached {
                        return Some((x as usize, y as usize));
                    }
                }
            }
            k += 2;
        }
        None
    }

    fn backward_step(&mut self, d: isize, check_meet: bool) -> Option<(usize, usize)> {
        let (n, m) = self.lens();
        let delta = n - m;
        let mut k = -d + self.trim[2];
        while k <= d - self.trim[3] {
            let at = (self.offset + k) as usize;
            let mut x = if k == -d || (k != d && self.backward[at - 1] < self.backward[at + 1]) {
                self.backward[at + 1]
            } else {
                self.backward[at - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && self.old[(n - x - 1) as usize] == self.new[(m - y - 1) as usize]
            {
                x += 1;
                y += 1;
            }
            self.backward[at] = x;
            if x > n {
                self.trim[3] += 2;
            } else if y > m {
                self.trim[2] += 2;
            } else if check_meet {
                let other = self.offset + delta - k;
                if other >= 0 && (other as usize) < self.forward.len() {
                    let forward_x = self.forward[other as usize];
                    if forward_x != -1 && forward_x >= n - x {
                        let forward_y = forward_x - (other - self.offset);
                        return Some((forward_x as usize, forward_y as usize));
                    }
                }
            }
            k += 2;
        }
        None
    }
}
