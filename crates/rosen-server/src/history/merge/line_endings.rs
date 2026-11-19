//! Newline conventions for the files a change contributes to a merge result
//! (R5, #397, #424). A text file whose `.gitattributes` in the target base
//! declares `eol=lf` or `eol=crlf` is written in that form, so a land both
//! repairs a file stored in the wrong form and undoes an editor's rewrite.
//! A file with no declared `eol` is written in the convention of the target
//! base's version of the path, or, for a new file, the one most of the
//! project's text files use, so an editor that saves a whole LF file with
//! CRLF does not land a whole-file rewrite. Binary, `-text`, generated,
//! symlink and gitlink entries keep their bytes.

use super::gitattributes::{Attributes, Declared};
use super::{HistoryResult, ObjectId};
use crate::history::trees::{apply, changed_files, Entry};
use rosen_core::FileMode;
use rosen_merge::{is_binary, normalize_line_endings, MergePolicy};
use rosen_protocol::LineEndingRewrite;
use rosen_store::ObjectStore;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};

/// Files above this size keep their bytes: a merge never reads a large
/// file whole just to rewrite its newlines.
const LARGEST_REWRITTEN: u64 = 16 << 20;
/// Text files read to find a project's prevailing convention.
const SAMPLED_FILES: usize = 16;
/// Tree entries looked at while sampling, so a tree of binaries or a deep
/// one stays cheap to sample.
const SAMPLE_VISITS: usize = 256;
/// Largest file read while sampling.
const LARGEST_SAMPLED: u64 = 1 << 20;

const LF: &[u8] = b"\n";
const CRLF: &[u8] = b"\r\n";

/// Decides the newline convention of each file a change contributes to a
/// merge onto one target tree.
pub(super) struct Conventions<'a> {
    store: &'a dyn ObjectStore,
    target: ObjectId,
    /// The target tree's `.gitattributes`, read as paths need them.
    attributes: Attributes<'a>,
    /// Off for merges that keep every byte; `keep` then changes nothing.
    enabled: bool,
    /// The target tree's prevailing convention, sampled at most once and
    /// only when a new file needs it. The inner `None` means no sampled
    /// file had a terminated line.
    prevailing: Option<Option<&'static [u8]>>,
}

impl<'a> Conventions<'a> {
    pub(super) fn new(store: &'a dyn ObjectStore, target: ObjectId, enabled: bool) -> Self {
        Self {
            store,
            target,
            attributes: Attributes::new(store, target),
            enabled,
            prevailing: None,
        }
    }

    /// `file`, at `path`, as the merge result holds it. A text file is
    /// rewritten in the newline its target `.gitattributes` declares, or
    /// without one in the convention of `reference`, the target's version
    /// of the path. Without a reference, or one with no terminated line,
    /// the project's prevailing convention applies. A `-text` file, or an
    /// undeclared one with a binary or oversized reference, stays as it is.
    pub(super) fn keep(
        &mut self,
        path: &str,
        file: Option<Entry>,
        reference: Option<Entry>,
        policy: MergePolicy,
    ) -> HistoryResult<Option<Entry>> {
        let Some(entry) = file.filter(|_| self.enabled && policy != MergePolicy::Generated) else {
            return Ok(file);
        };
        let Some(text) = text_of(self.store, entry, LARGEST_REWRITTEN)? else {
            return Ok(file);
        };
        if !text.contains(&b'\n') {
            return Ok(file);
        }
        let convention = match self.attributes.declared(path)? {
            Declared::NotText => None,
            Declared::Eol(eol) => Some(eol.newline()),
            Declared::Nothing => self.undeclared(reference)?,
        };
        let Some(convention) = convention else {
            return Ok(file);
        };
        Ok(match normalize_line_endings(&text, convention) {
            Cow::Borrowed(_) => file,
            Cow::Owned(kept) => Some(Entry {
                mode: entry.mode,
                id: self.store.put_blob(&kept)?,
            }),
        })
    }

    /// The convention of a file no attribute declares: its reference's, or
    /// the project's prevailing one. `None` keeps the file's bytes.
    fn undeclared(&mut self, reference: Option<Entry>) -> HistoryResult<Option<&'static [u8]>> {
        let reference = match reference {
            Some(reference) => match text_of(self.store, reference, LARGEST_REWRITTEN)? {
                Some(base) => convention_of(&base),
                None => return Ok(None),
            },
            None => None,
        };
        // Sample the project only when the base version gives no convention.
        match reference {
            Some(convention) => Ok(Some(convention)),
            None => self.prevailing(),
        }
    }

    fn prevailing(&mut self) -> HistoryResult<Option<&'static [u8]>> {
        if let Some(prevailing) = self.prevailing {
            return Ok(prevailing);
        }
        let sampled = self.sample()?;
        self.prevailing = Some(sampled);
        Ok(sampled)
    }

    /// The convention most sampled text files in the target use; LF on a
    /// tie. Sampling reads a bounded number of entries depth first.
    fn sample(&self) -> HistoryResult<Option<&'static [u8]>> {
        let (mut crlf, mut lf, mut visits) = (0usize, 0usize, 0usize);
        let mut trees = vec![self.target];
        'walk: while let Some(tree) = trees.pop() {
            for entry in self.store.read_tree(&tree)?.entries() {
                if crlf + lf >= SAMPLED_FILES || visits >= SAMPLE_VISITS {
                    break 'walk;
                }
                visits += 1;
                if entry.mode.is_tree() {
                    trees.push(entry.id);
                    continue;
                }
                let file = Entry {
                    mode: entry.mode,
                    id: entry.id,
                };
                match text_of(self.store, file, LARGEST_SAMPLED)?
                    .as_deref()
                    .and_then(convention_of)
                {
                    Some(newline) if newline == CRLF => crlf += 1,
                    Some(_) => lf += 1,
                    None => {}
                }
            }
        }
        Ok(match (crlf, lf) {
            (0, 0) => None,
            _ if crlf > lf => Some(CRLF),
            _ => Some(LF),
        })
    }
}

/// A regular file's bytes, when it is text no larger than `limit`.
fn text_of(store: &dyn ObjectStore, entry: Entry, limit: u64) -> HistoryResult<Option<Vec<u8>>> {
    let regular = matches!(
        entry.mode,
        FileMode::Regular | FileMode::Executable | FileMode::Legacy(_)
    );
    if !regular || store.object_size(&entry.id)? > limit {
        return Ok(None);
    }
    let data = store.read_blob(&entry.id)?.data;
    Ok((!is_binary(&data)).then_some(data))
}

/// The newline of `text`'s first terminated line, as R5 reads a file's
/// convention.
fn convention_of(text: &[u8]) -> Option<&'static [u8]> {
    let at = text.iter().position(|&byte| byte == b'\n')?;
    Some(if at > 0 && text[at - 1] == b'\r' {
        CRLF
    } else {
        LF
    })
}

/// `head` with each text file it changed since `base` written in the newline
/// [`Conventions::keep`] gives it on `base`: the merge result when nothing
/// landed under the change. The head tree itself comes back when nothing
/// needs rewriting.
pub(super) fn keep_in_tree(
    store: &dyn ObjectStore,
    base: ObjectId,
    head: ObjectId,
    policy: &dyn Fn(&str) -> MergePolicy,
) -> HistoryResult<ObjectId> {
    let mut conventions = Conventions::new(store, base, true);
    let mut rewrites = BTreeMap::new();
    for change in changed_files(store, Some(base), Some(head), &[])? {
        let policy = policy(&change.path);
        let kept = conventions.keep(&change.path, change.new, change.old, policy)?;
        if kept != change.new {
            rewrites.insert(change.path, kept);
        }
    }
    if rewrites.is_empty() {
        return Ok(head);
    }
    // Every rewritten path is already a file in `head`, so none collides.
    let mut collisions = Vec::new();
    Ok(apply(store, Some(head), &rewrites, &mut collisions)?.unwrap_or(head))
}

/// The files `merged`, a merge result on the tree `base`, holds in other
/// line endings than the change's `head` tree gave them: what the newline
/// rule rewrote in what the change wrote, so `rosen land --check` and the
/// land answer show it (#424). Only paths the merge changed on `base` count.
pub(super) fn rewritten(
    store: &dyn ObjectStore,
    [base, merged, head]: [ObjectId; 3],
) -> HistoryResult<Vec<LineEndingRewrite>> {
    let landed: HashSet<String> = changed_files(store, Some(base), Some(merged), &[])?
        .into_iter()
        .map(|change| change.path)
        .collect();
    let mut rewrites = Vec::new();
    if landed.is_empty() {
        return Ok(rewrites);
    }
    for change in changed_files(store, Some(head), Some(merged), &[])? {
        let (Some(wrote), Some(kept)) = (change.old, change.new) else {
            continue;
        };
        if !landed.contains(&change.path) {
            continue;
        }
        let Some(kept) = text_of(store, kept, LARGEST_REWRITTEN)? else {
            continue;
        };
        let Some(newline) = convention_of(&kept) else {
            continue;
        };
        let Some(wrote) = text_of(store, wrote, LARGEST_REWRITTEN)? else {
            continue;
        };
        if let Cow::Owned(_) = normalize_line_endings(&wrote, newline) {
            let eol = if newline == CRLF { "crlf" } else { "lf" };
            rewrites.push(LineEndingRewrite {
                path: change.path,
                eol: eol.to_owned(),
            });
        }
    }
    Ok(rewrites)
}
