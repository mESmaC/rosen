//! Target parsing and surgical `rosen.toml` edits for `rosen collapse link`
//! and `rosen collapse unlink` (SPEC R1, R10, #437).
//!
//! Edits preserve existing comments, formatting and unrelated settings, and
//! every result is verified with [`ProjectConfig::parse`] before it is written.

use rosen_core::ProjectConfig;

/// Default Collapse API origin when `--host` is omitted.
pub const DEFAULT_COLLAPSE_HOST: &str = "https://api.collapse.sh";

/// Validated Collapse repository target and the URLs derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapseTarget {
    pub origin: String,
    pub owner: String,
    pub name: String,
    pub collapse_root: String,
    pub mirror_remote: String,
    pub state_url: String,
    pub ledger_url: String,
}

/// Result of planning a `rosen.toml` edit against the current base text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEdit {
    Unchanged,
    Write(String),
    Delete,
}

impl CollapseTarget {
    /// Parses `<owner>/<name>` and `--host <url>` into validated Collapse URLs.
    pub fn parse(repo: &str, host: &str) -> Result<Self, String> {
        let (owner, name) = parse_repo_slug(repo)?;
        let origin = parse_host_origin(host)?;
        let collapse_root = format!("{origin}/{owner}/{name}");
        let mirror_remote = format!("{collapse_root}.git");
        let state_url = format!("{collapse_root}/rosen/state");
        let ledger_url = format!("{collapse_root}/ledgers");
        // Verify both URLs against rosen.toml's own validator up front.
        let check_toml =
            format!("collapse = {collapse_root:?}\n[mirror]\nremote = {mirror_remote:?}\n");
        ProjectConfig::parse(&check_toml).map_err(|err| err.to_string())?;
        Ok(Self {
            origin,
            owner,
            name,
            collapse_root,
            mirror_remote,
            state_url,
            ledger_url,
        })
    }

    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The three Collapse HTTPS paths whose git credentials must be approved.
    pub fn credential_urls(&self) -> [&str; 3] {
        [
            self.mirror_remote.as_str(),
            self.state_url.as_str(),
            self.ledger_url.as_str(),
        ]
    }
}

fn parse_repo_slug(repo: &str) -> Result<(String, String), String> {
    let trimmed = repo.trim().trim_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!(
            "repository {repo:?} must be `<owner>/<name>` (for example `mESmaC/example`)"
        ));
    }
    let (owner, name) = (parts[0], parts[1]);
    if let Some(stripped) = name.strip_suffix(".git") {
        return Err(format!(
            "repository {repo:?} must not end with `.git`; pass `{owner}/{stripped}`"
        ));
    }
    for (label, part) in [("owner", owner), ("name", name)] {
        let valid = !matches!(part, "." | "..")
            && part
                .chars()
                .all(|c| c.is_ascii_graphic() && !matches!(c, '"' | '\\' | '?' | '#' | '@' | '/'));
        if !valid {
            return Err(format!(
                "repository {label} {part:?} contains invalid characters"
            ));
        }
    }
    Ok((owner.to_owned(), name.to_owned()))
}

fn parse_host_origin(host: &str) -> Result<String, String> {
    let trimmed = host.trim().trim_end_matches('/');
    let Some(rest) = trimmed.strip_prefix("https://") else {
        return Err(
            "--host must be an `https://` origin (for example `https://api.collapse.sh`)"
                .to_owned(),
        );
    };
    if rest.is_empty() {
        return Err("--host is missing a host name".to_owned());
    }
    if rest.contains('@') {
        return Err("--host must not carry credentials; provide the token on stdin or in ROSEN_COLLAPSE_TOKEN".to_owned());
    }
    if rest.contains(['/', '?', '#']) {
        return Err(
            "--host must be an origin with no path (for example `https://api.collapse.sh`)"
                .to_owned(),
        );
    }
    if !rest
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
    {
        return Err("--host contains invalid characters".to_owned());
    }
    Ok(trimmed.to_owned())
}

/// Sets `collapse` and `[mirror].remote` in `rosen.toml`, preserving existing
/// comments and unrelated keys.
pub fn set_collapse_and_mirror(
    existing: Option<&str>,
    collapse_url: &str,
    mirror_url: &str,
) -> Result<ConfigEdit, String> {
    let Some(text) = existing else {
        let created = format!("collapse = {collapse_url:?}\n\n[mirror]\nremote = {mirror_url:?}\n");
        verify_linked(&created, collapse_url, mirror_url)?;
        return Ok(ConfigEdit::Write(created));
    };
    if let Ok(parsed) = ProjectConfig::parse(text) {
        if parsed.collapse.as_deref() == Some(collapse_url)
            && parsed.mirror.remote.as_deref() == Some(mirror_url)
        {
            return Ok(ConfigEdit::Unchanged);
        }
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<String> = split_lines(text);
    let updated = apply_set(&lines, collapse_url, mirror_url);
    let rendered = join_lines(&updated, eol);
    verify_linked(&rendered, collapse_url, mirror_url)?;
    Ok(if rendered == text {
        ConfigEdit::Unchanged
    } else {
        ConfigEdit::Write(rendered)
    })
}

/// Removes `collapse` and `[mirror].remote` from `rosen.toml`, deleting the
/// file only when nothing else remains in it.
pub fn remove_collapse_and_mirror(existing: Option<&str>) -> Result<ConfigEdit, String> {
    let Some(text) = existing else {
        return Ok(ConfigEdit::Unchanged);
    };
    if let Ok(parsed) = ProjectConfig::parse(text) {
        if parsed.collapse.is_none() && parsed.mirror.remote.is_none() {
            return Ok(ConfigEdit::Unchanged);
        }
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines = split_lines(text);
    let updated = apply_remove(&lines);
    if updated.iter().all(|line| line.trim().is_empty()) {
        return Ok(ConfigEdit::Delete);
    }
    let rendered = join_lines(&updated, eol);
    let parsed = ProjectConfig::parse(&rendered).map_err(|err| err.to_string())?;
    if parsed.collapse.is_some() || parsed.mirror.remote.is_some() {
        return Err("could not remove `collapse` and `[mirror].remote` from rosen.toml".to_owned());
    }
    Ok(if rendered == text {
        ConfigEdit::Unchanged
    } else {
        ConfigEdit::Write(rendered)
    })
}

fn verify_linked(text: &str, collapse_url: &str, mirror_url: &str) -> Result<(), String> {
    let parsed = ProjectConfig::parse(text).map_err(|err| err.to_string())?;
    if parsed.collapse.as_deref() != Some(collapse_url)
        || parsed.mirror.remote.as_deref() != Some(mirror_url)
    {
        return Err("updated rosen.toml did not resolve to the requested Collapse URLs".to_owned());
    }
    Ok(())
}

/// Scans TOML lines outside multi-line arrays/strings, classifying each line's
/// section and top-level key name.
#[derive(Default)]
struct TomlScanner {
    section: Option<String>,
    bracket_depth: usize,
    triple_quote: Option<&'static [u8]>,
}

#[derive(Debug, PartialEq, Eq)]
enum LineKind {
    Section(String),
    Key {
        section: Option<String>,
        key: String,
    },
    Other,
}

impl TomlScanner {
    fn scan(&mut self, line: &str) -> LineKind {
        if self.triple_quote.is_some() || self.bracket_depth > 0 {
            self.advance_multiline(line.as_bytes());
            return LineKind::Other;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return LineKind::Other;
        }
        if let Some(section) = parse_section_header(trimmed) {
            self.section = Some(section.to_owned());
            return LineKind::Section(section.to_owned());
        }
        let kind = parse_key_name(trimmed).map_or(LineKind::Other, |key| LineKind::Key {
            section: self.section.clone(),
            key,
        });
        self.advance_multiline(trimmed.as_bytes());
        kind
    }

    fn advance_multiline(&mut self, bytes: &[u8]) {
        let mut i = 0;
        let mut in_single = false;
        let mut in_double = false;
        while i < bytes.len() {
            if let Some(delim) = self.triple_quote {
                if bytes[i..].starts_with(delim) {
                    self.triple_quote = None;
                    i += 3;
                    continue;
                }
                i += if delim == b"\"\"\"" && bytes[i] == b'\\' {
                    2
                } else {
                    1
                };
                continue;
            }
            if in_double {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    in_double = false;
                }
                i += 1;
                continue;
            }
            if in_single {
                if bytes[i] == b'\'' {
                    in_single = false;
                }
                i += 1;
                continue;
            }
            if bytes[i] == b'#' {
                break;
            }
            if bytes[i..].starts_with(b"\"\"\"") {
                self.triple_quote = Some(b"\"\"\"");
                i += 3;
            } else if bytes[i..].starts_with(b"'''") {
                self.triple_quote = Some(b"'''");
                i += 3;
            } else if bytes[i] == b'"' {
                in_double = true;
                i += 1;
            } else if bytes[i] == b'\'' {
                in_single = true;
                i += 1;
            } else {
                if bytes[i] == b'[' {
                    self.bracket_depth += 1;
                } else if bytes[i] == b']' {
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                }
                i += 1;
            }
        }
    }
}

fn parse_section_header(trimmed: &str) -> Option<&str> {
    if !trimmed.starts_with('[') || trimmed.starts_with("[[") {
        return None;
    }
    let without_comment = trimmed.split('#').next().unwrap_or(trimmed).trim();
    let inner = without_comment.strip_prefix('[')?.strip_suffix(']')?.trim();
    (!inner.is_empty()).then_some(inner)
}

fn parse_key_name(trimmed: &str) -> Option<String> {
    let (lhs, _) = trimmed.split_once('=')?;
    let parts: Vec<&str> = lhs
        .split('.')
        .map(|part| part.trim().trim_matches('"').trim_matches('\''))
        .collect();
    if parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    Some(parts.join("."))
}

fn apply_set(lines: &[String], collapse_url: &str, mirror_url: &str) -> Vec<String> {
    let collapse_line = format!("collapse = {collapse_url:?}");
    let remote_line = format!("remote = {mirror_url:?}");
    let mut out = Vec::with_capacity(lines.len() + 4);
    let mut scanner = TomlScanner::default();
    let mut collapse_written = false;
    let mut mirror_written = false;
    let mut mirror_header_idx: Option<usize> = None;

    for line in lines {
        match scanner.scan(line) {
            LineKind::Section(name) => {
                if !collapse_written {
                    insert_before_table(&mut out, collapse_line.clone());
                    collapse_written = true;
                }
                if name == "mirror" {
                    mirror_header_idx = Some(out.len());
                }
                out.push(line.clone());
            }
            LineKind::Key { section: None, key } if key == "collapse" => {
                out.push(collapse_line.clone());
                collapse_written = true;
            }
            LineKind::Key { section: None, key } if key == "mirror.remote" => {
                out.push(format!("mirror.remote = {mirror_url:?}"));
                mirror_written = true;
            }
            LineKind::Key { section: None, key } if key == "mirror" => {
                out.push(format!("mirror = {{ remote = {mirror_url:?} }}"));
                mirror_written = true;
            }
            LineKind::Key {
                section: Some(sec),
                key,
            } if sec == "mirror" && key == "remote" => {
                out.push(remote_line.clone());
                mirror_written = true;
            }
            _ => out.push(line.clone()),
        }
    }
    if !collapse_written {
        push_separated(&mut out, collapse_line);
    }
    if !mirror_written {
        if let Some(idx) = mirror_header_idx {
            out.insert(idx + 1, remote_line);
        } else {
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push("[mirror]".to_owned());
            out.push(remote_line);
        }
    }
    out
}

fn apply_remove(lines: &[String]) -> Vec<String> {
    let mut scanner = TomlScanner::default();
    let classified: Vec<(LineKind, &String)> = lines
        .iter()
        .map(|line| (scanner.scan(line), line))
        .collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut idx = 0;
    while idx < classified.len() {
        match &classified[idx].0 {
            LineKind::Key { section: None, key }
                if matches!(key.as_str(), "collapse" | "mirror.remote" | "mirror") =>
            {
                idx += 1;
            }
            LineKind::Section(name) if name == "mirror" => {
                let end = next_section(&classified, idx + 1);
                let kept_body = remove_mirror_remote(&classified[idx + 1..end]);
                if kept_body
                    .iter()
                    .any(|(kind, _)| matches!(kind, LineKind::Key { .. }))
                {
                    out.push(classified[idx].1.clone());
                    out.extend(kept_body.into_iter().map(|(_, line)| line.clone()));
                } else {
                    // Drop the now-empty `[mirror]` header and any trailing blank separator.
                    while out.last().is_some_and(|l| l.trim().is_empty()) {
                        out.pop();
                    }
                    let trailing_blank = end < classified.len()
                        && !out.is_empty()
                        && !out.last().is_some_and(|l| l.trim().is_empty());
                    if trailing_blank {
                        out.push(String::new());
                    }
                }
                idx = end;
            }
            _ => {
                out.push(classified[idx].1.clone());
                idx += 1;
            }
        }
    }
    trim_edge_blanks(&mut out);
    out
}

fn next_section(classified: &[(LineKind, &String)], start: usize) -> usize {
    classified[start..]
        .iter()
        .position(|(kind, _)| matches!(kind, LineKind::Section(_)))
        .map_or(classified.len(), |offset| start + offset)
}

fn remove_mirror_remote<'a>(
    section: &'a [(LineKind, &'a String)],
) -> Vec<(&'a LineKind, &'a String)> {
    section
        .iter()
        .filter(|(kind, _)| !matches!(kind, LineKind::Key { key, .. } if key == "remote"))
        .map(|(kind, line)| (kind, *line))
        .collect()
}

fn insert_before_table(out: &mut Vec<String>, line: String) {
    let mut trailing_blanks = 0;
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
        trailing_blanks += 1;
    }
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push(line);
    for _ in 0..trailing_blanks.max(1) {
        out.push(String::new());
    }
}

fn push_separated(out: &mut Vec<String>, line: String) {
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.push(line);
}

fn trim_edge_blanks(out: &mut Vec<String>) {
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
}

fn split_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect()
}

fn join_lines(lines: &[String], eol: &str) -> String {
    let mut joined = lines.join(eol);
    if !joined.is_empty() {
        joined.push_str(eol);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_and_host_and_rejects_bad_inputs() {
        let target = CollapseTarget::parse("mESmaC/example", "https://api.collapse.sh/").unwrap();
        assert_eq!(
            target.collapse_root,
            "https://api.collapse.sh/mESmaC/example"
        );
        assert_eq!(
            target.mirror_remote,
            "https://api.collapse.sh/mESmaC/example.git"
        );
        assert_eq!(
            target.state_url,
            "https://api.collapse.sh/mESmaC/example/rosen/state"
        );
        assert_eq!(
            target.ledger_url,
            "https://api.collapse.sh/mESmaC/example/ledgers"
        );

        for (repo, host) in [
            ("onlyowner", "https://api.collapse.sh"),
            ("a/b/c", "https://api.collapse.sh"),
            ("a/b.git", "https://api.collapse.sh"),
            ("a/..", "https://api.collapse.sh"),
            ("a/b", "http://api.collapse.sh"),
            ("a/b", "https://tok@api.collapse.sh"),
            ("a/b", "https://api.collapse.sh/extra"),
        ] {
            assert!(
                CollapseTarget::parse(repo, host).is_err(),
                "accepted ({repo:?}, {host:?})"
            );
        }
    }

    /// Risk (#437): linking and unlinking must preserve unrelated rosen.toml
    /// comments and tables while keeping the file valid under ProjectConfig.
    #[test]
    fn link_and_unlink_preserve_existing_config_and_round_trip() {
        let existing = "\
# Project config
review = \"after-land\"

ignore = [
    \".env\",
]

[warm_pool]
size = 6

[mirror]
remote = \"https://github.com/mESmaC/example.git\"
";
        let ConfigEdit::Write(linked) = set_collapse_and_mirror(
            Some(existing),
            "https://api.collapse.sh/mESmaC/example",
            "https://api.collapse.sh/mESmaC/example.git",
        )
        .unwrap() else {
            panic!("expected Write");
        };
        assert!(linked.contains("# Project config"), "{linked}");
        assert!(linked.contains("size = 6"), "{linked}");
        let parsed = ProjectConfig::parse(&linked).unwrap();
        assert_eq!(
            parsed.collapse.as_deref(),
            Some("https://api.collapse.sh/mESmaC/example")
        );
        assert_eq!(
            parsed.mirror.remote.as_deref(),
            Some("https://api.collapse.sh/mESmaC/example.git")
        );
        assert_eq!(parsed.warm_pool.size, 6);

        // Idempotent when already linked.
        assert_eq!(
            set_collapse_and_mirror(
                Some(&linked),
                "https://api.collapse.sh/mESmaC/example",
                "https://api.collapse.sh/mESmaC/example.git",
            )
            .unwrap(),
            ConfigEdit::Unchanged
        );

        // Unlinking removes both settings and preserves everything else.
        let ConfigEdit::Write(unlinked) = remove_collapse_and_mirror(Some(&linked)).unwrap() else {
            panic!("expected Write");
        };
        assert!(unlinked.contains("# Project config"), "{unlinked}");
        assert!(unlinked.contains("size = 6"), "{unlinked}");
        assert!(!unlinked.contains("[mirror]"), "{unlinked}");
        let parsed = ProjectConfig::parse(&unlinked).unwrap();
        assert_eq!(parsed.collapse, None);
        assert_eq!(parsed.mirror.remote, None);
        assert_eq!(parsed.warm_pool.size, 6);
        assert_eq!(
            remove_collapse_and_mirror(Some(&unlinked)).unwrap(),
            ConfigEdit::Unchanged
        );

        // Creating from scratch and unlinking deletes the now-empty file.
        let ConfigEdit::Write(fresh) = set_collapse_and_mirror(
            None,
            "https://api.collapse.sh/mESmaC/example",
            "https://api.collapse.sh/mESmaC/example.git",
        )
        .unwrap() else {
            panic!("expected Write");
        };
        assert_eq!(
            remove_collapse_and_mirror(Some(&fresh)).unwrap(),
            ConfigEdit::Delete
        );
    }
}
