use super::*;
use rosen_platform::unix_delete::Directory;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek};
use std::rc::Rc;

struct Entry {
    parent: Rc<Directory>,
    name: OsString,
    rel: String,
    // Pin directory identities; ordinary files are reopened and reverified
    // just before removal so large workspaces do not exhaust Unix fd limits.
    directory: Option<File>,
    expected: Option<ObjectId>,
}

impl Entry {
    fn remove(self) -> io::Result<()> {
        let file = match self.directory {
            Some(file) => file,
            None => {
                let file = self
                    .parent
                    .child(&self.name)
                    .map_err(|err| kept(&self.rel, err))?;
                if !file.metadata()?.is_file() {
                    return Err(kept(&self.rel, "not a plain file"));
                }
                verify(&file, self.expected).map_err(|err| kept(&self.rel, err))?;
                file
            }
        };
        self.parent
            .remove_checked(&self.name, &file, |file| verify(file, self.expected))
            .map_err(|err| kept(&self.rel, err))
    }
}

pub(crate) fn remove_verified(
    root: &Path,
    expected: &BTreeMap<String, ObjectId>,
    mut ignored: impl FnMut(&str, bool) -> bool,
) -> io::Result<()> {
    let parent = Rc::new(Directory::open(
        root.parent().ok_or_else(|| kept("", "root directory"))?,
    )?);
    let name = root
        .file_name()
        .ok_or_else(|| kept("", "root directory"))?
        .to_owned();
    let directory = Rc::new(Directory::from_file(parent.child(&name)?)?);
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    plan(
        directory.clone(),
        "",
        expected,
        &mut ignored,
        &mut files,
        &mut dirs,
    )?;
    remove_plan(&directory, files, dirs)?;
    parent.remove_checked(&name, directory.file(), |_| Ok(()))?;
    Ok(())
}

fn remove_plan(directory: &Directory, files: Vec<Entry>, mut dirs: Vec<Entry>) -> io::Result<()> {
    dirs.sort_by_key(|entry| std::cmp::Reverse(entry.rel.matches('/').count()));
    let (state, user): (Vec<_>, Vec<_>) = files
        .into_iter()
        .partition(|e| e.rel.starts_with(".rosen/"));
    let (state_dirs, user_dirs): (Vec<_>, Vec<_>) = dirs
        .into_iter()
        .partition(|e| e.rel == ".rosen" || e.rel.starts_with(".rosen/"));
    for entry in user.into_iter().chain(user_dirs) {
        entry.remove()?;
    }
    let remaining: Vec<_> = directory
        .names()?
        .into_iter()
        .filter(|name| name != ".rosen")
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if !remaining.is_empty() {
        return Err(kept(
            "",
            format!(
                "new files appeared during deletion: {}",
                remaining.join(", ")
            ),
        ));
    }
    // Recovery information goes only after all user directories are empty.
    let mut state = state;
    state.sort_by_key(|entry| entry.rel == ".rosen/workspace.json");
    for entry in state.into_iter().chain(state_dirs) {
        entry.remove()?;
    }
    Ok(())
}

fn plan(
    dir: Rc<Directory>,
    prefix: &str,
    expected: &BTreeMap<String, ObjectId>,
    ignored: &mut impl FnMut(&str, bool) -> bool,
    files: &mut Vec<Entry>,
    dirs: &mut Vec<Entry>,
) -> io::Result<()> {
    for name in dir.names()? {
        let text = name
            .to_str()
            .ok_or_else(|| kept(prefix, "non-UTF-8 file name"))?;
        let rel = if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}/{text}")
        };
        let file = dir.child(&name).map_err(|err| kept(&rel, err))?;
        let meta = file.metadata()?;
        if !meta.is_file() && !meta.is_dir() {
            return Err(kept(&rel, "not a plain file or directory"));
        }
        let entry = Entry {
            parent: dir.clone(),
            name,
            rel,
            directory: if meta.is_dir() {
                Some(file.try_clone()?)
            } else {
                None
            },
            expected: None,
        };
        if meta.is_dir() {
            let child = check_directory(&entry, file)?;
            plan(child, &entry.rel, expected, ignored, files, dirs)?;
            dirs.push(entry);
        } else {
            files.push(check_file(entry, &file, expected, ignored)?);
        }
    }
    Ok(())
}

fn check_directory(entry: &Entry, file: File) -> io::Result<Rc<Directory>> {
    let child = Rc::new(Directory::from_file(file)?);
    // An ignore pattern must never turn a previous attempt's recovery bytes
    // disposable, even when it matches the private isolation directory.
    if entry.name.to_string_lossy().starts_with(".rosen-remove-") && !child.names()?.is_empty() {
        return Err(kept(
            &entry.rel,
            "interrupted isolation; recover retained entries first",
        ));
    }
    reject_nested(&child, &entry.rel)?;
    Ok(child)
}

fn check_file(
    mut entry: Entry,
    file: &File,
    expected: &BTreeMap<String, ObjectId>,
    ignored: &mut impl FnMut(&str, bool) -> bool,
) -> io::Result<Entry> {
    if !entry.rel.starts_with(".rosen/") && !ignored(&entry.rel, false) {
        entry.expected = Some(
            *expected
                .get(&entry.rel)
                .ok_or_else(|| kept(&entry.rel, "unstored file"))?,
        );
        verify(file, entry.expected).map_err(|err| kept(&entry.rel, err))?;
    }
    Ok(entry)
}

fn reject_nested(dir: &Directory, rel: &str) -> io::Result<()> {
    if let Ok(state) = dir.child(std::ffi::OsStr::new(".rosen")) {
        let state = Directory::from_file(state)?;
        if state.names()?.iter().any(|name| name == "workspace.json") {
            return Err(kept(rel, "nested workspace"));
        }
    }
    Ok(())
}

fn verify(mut file: &File, expected: Option<ObjectId>) -> io::Result<()> {
    if let Some(expected) = expected {
        file.rewind()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if ObjectId::hash_object("blob", &bytes) != expected {
            return Err(io::Error::other("unstored file"));
        }
    }
    Ok(())
}

fn kept(rel: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("workspace kept; {rel}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // R4 risk (#375): a durable Unix workspace must be removable, while
    // bytes absent from the server must keep both the edit and its marker.
    #[test]
    fn final_removal_matches_deletion_fixtures() {
        for line in include_str!("../../../../../proofs/fixtures/workspace-deletion.tsv").lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 5 || fields[0] != "0" || fields[1] != "0" {
                continue;
            }
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("work");
            std::fs::create_dir_all(workspace.join(".rosen")).unwrap();
            std::fs::write(workspace.join(".rosen/workspace.json"), b"marker").unwrap();
            std::fs::write(workspace.join("file"), b"stored").unwrap();
            let stored = if fields[2] == "1" {
                b"other!"
            } else {
                b"stored"
            };
            let expected = BTreeMap::from([("file".into(), ObjectId::hash_object("blob", stored))]);
            let result = remove_verified(&workspace, &expected, |_, _| fields[3] == "1");
            assert_eq!(result.is_ok(), fields[4] == "1", "{line}: {result:?}");
            if result.is_err() {
                assert_eq!(std::fs::read(workspace.join("file")).unwrap(), b"stored");
                assert!(workspace.join(".rosen/workspace.json").exists());
            } else {
                assert!(!workspace.exists());
            }
        }
    }

    // R4 risk: after a refusal or crash an ignore rule matching the hidden
    // isolation directory must not authorize deletion of a retained edit.
    #[test]
    fn interrupted_isolation_survives_ignored_paths() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("work");
        let recovery = workspace.join(".rosen-remove-interrupted");
        std::fs::create_dir_all(&recovery).unwrap();
        std::fs::write(recovery.join("entry"), b"unstored replacement").unwrap();
        assert!(remove_verified(&workspace, &BTreeMap::new(), |_, _| true).is_err());
        assert_eq!(
            std::fs::read(recovery.join("entry")).unwrap(),
            b"unstored replacement"
        );
    }
}
