//! Sparse checkout (SPEC.md R3): a workspace that holds only chosen paths.

use super::MaterialiseError;

/// Which paths a workspace holds. A chosen path holds itself and, when it is
/// a directory, everything under it. Directories on the way to a chosen path
/// are created, but hold nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sparse {
    /// Normalised `/`-separated paths, or empty for the whole tree.
    paths: Vec<Vec<u8>>,
}

impl Sparse {
    /// The whole tree.
    pub fn all() -> Self {
        Self::default()
    }

    /// Only `paths` and what lies under them. Both `/` and `\` separate
    /// components, and leading `./` or `/` is ignored. An empty list means the
    /// whole tree.
    pub fn paths<I, S>(paths: I) -> Result<Self, MaterialiseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let paths = paths
            .into_iter()
            .map(|path| normalise(path.as_ref()))
            .collect::<Result<_, _>>()?;
        Ok(Self { paths })
    }

    /// The chosen paths, normalised; empty for the whole tree.
    pub fn chosen(&self) -> Vec<String> {
        self.paths
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect()
    }

    pub fn is_all(&self) -> bool {
        self.paths.is_empty()
    }

    /// Whether the workspace holds the file or directory at `path`.
    pub fn includes(&self, path: &[u8]) -> bool {
        self.is_all() || self.paths.iter().any(|chosen| is_within(path, chosen))
    }

    /// Whether the directory at `dir` must exist: it is included, or a chosen
    /// path lies under it.
    pub fn enters(&self, dir: &[u8]) -> bool {
        self.includes(dir) || self.paths.iter().any(|chosen| is_within(chosen, dir))
    }
}

/// Whether `path` is `ancestor` or lies under it.
fn is_within(path: &[u8], ancestor: &[u8]) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|rest| rest.is_empty() || rest[0] == b'/')
}

fn normalise(path: &str) -> Result<Vec<u8>, MaterialiseError> {
    let components: Vec<&str> = path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    if components.is_empty() || components.contains(&"..") {
        return Err(MaterialiseError::InvalidSparsePath(path.to_owned()));
    }
    Ok(components.join("/").into_bytes())
}
