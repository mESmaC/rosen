//! Target and checker platform identifiers (SPEC R12, #440).
//!
//! A platform names an operating system (`windows`, `macos`, `linux`) with an
//! optional architecture (`x86_64`, `arm64`, …), formatted as `<os>` or
//! `<os>-<arch>`. Checkers report their host OS and architecture; a check set
//! may require either OS-level platforms (`["windows", "macos", "linux"]`) or
//! architecture-specific ones (`["macos-arm64"]`).

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Canonical platform identifier (`<os>` or `<os>-<arch>`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Platform {
    canonical: String,
    os_len: usize,
}

impl Platform {
    /// Parses and normalises a platform string (`windows`, `macos`, `linux`,
    /// optionally followed by `-<arch>`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("platform must not be empty".to_owned());
        }
        let lower = trimmed.to_ascii_lowercase();
        let (raw_os, raw_arch) = match lower.split_once('-') {
            Some((os, arch)) => (os, Some(arch)),
            None => (lower.as_str(), None),
        };
        let os = normalize_os(raw_os).ok_or_else(|| {
            format!("unknown platform OS {raw_os:?}; expected windows, macos or linux")
        })?;
        let canonical = match raw_arch {
            Some(arch) => {
                let arch = normalize_arch(arch)?;
                format!("{os}-{arch}")
            }
            None => os.to_owned(),
        };
        Ok(Self {
            os_len: os.len(),
            canonical,
        })
    }

    /// The current machine's OS and architecture (for example `windows-x86_64`
    /// or `macos-arm64`).
    pub fn host() -> Self {
        let os = normalize_os(std::env::consts::OS).unwrap_or("linux");
        let arch = normalize_arch(std::env::consts::ARCH).unwrap_or(std::env::consts::ARCH);
        let canonical = format!("{os}-{arch}");
        Self {
            os_len: os.len(),
            canonical,
        }
    }

    /// Combines an OS-only override with the host architecture when the OS
    /// matches this machine, so `--platform macos` on an Apple Silicon Mac
    /// still reports `macos-arm64`.
    pub fn resolve_override(raw: Option<&str>) -> Result<Self, String> {
        let Some(raw) = raw else {
            return Ok(Self::host());
        };
        let parsed = Self::parse(raw)?;
        let host = Self::host();
        if parsed.arch().is_none() && parsed.os() == host.os() {
            Ok(host)
        } else {
            Ok(parsed)
        }
    }

    pub fn os(&self) -> &str {
        &self.canonical[..self.os_len]
    }

    pub fn arch(&self) -> Option<&str> {
        (self.canonical.len() > self.os_len).then(|| &self.canonical[self.os_len + 1..])
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    /// Whether a checker running on `self` satisfies a required `target`.
    pub fn matches(&self, target: &Platform) -> bool {
        self.os() == target.os() && (target.arch().is_none() || self.arch() == target.arch())
    }

    /// Human-readable label (`Windows`, `macOS`, `Linux`, with architecture
    /// when constrained).
    pub fn display_name(&self) -> String {
        let os = match self.os() {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            other => other,
        };
        match self.arch() {
            Some(arch) => format!("{os} ({arch})"),
            None => os.to_owned(),
        }
    }

    /// Human-readable label for a platform string stored on the wire.
    pub fn display_label(raw: &str) -> String {
        Self::parse(raw).map_or_else(|_| raw.to_owned(), |p| p.display_name())
    }
}

fn normalize_os(raw: &str) -> Option<&'static str> {
    match raw {
        "windows" | "win" | "win32" => Some("windows"),
        "macos" | "darwin" | "osx" => Some("macos"),
        "linux" => Some("linux"),
        _ => None,
    }
}

fn normalize_arch(raw: &str) -> Result<&str, String> {
    if raw.is_empty()
        || !raw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(format!("invalid platform architecture {raw:?}"));
    }
    Ok(match raw {
        "aarch64" => "arm64",
        "amd64" => "x86_64",
        other => other,
    })
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl FromStr for Platform {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Platform {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical)
    }
}

impl<'de> Deserialize<'de> for Platform {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_normalizes_and_matches_platforms() {
        let mac_arm = Platform::parse("macos-aarch64").unwrap();
        assert_eq!(mac_arm.as_str(), "macos-arm64");
        assert_eq!(mac_arm.os(), "macos");
        assert_eq!(mac_arm.arch(), Some("arm64"));

        let mac = Platform::parse("macOS").unwrap();
        assert_eq!(mac.as_str(), "macos");
        assert_eq!(mac.display_name(), "macOS");
        assert!(mac_arm.matches(&mac));
        assert!(mac_arm.matches(&Platform::parse("macos-arm64").unwrap()));
        assert!(!mac_arm.matches(&Platform::parse("macos-x86_64").unwrap()));
        assert!(!mac.matches(&mac_arm));

        let win = Platform::parse("windows-amd64").unwrap();
        assert_eq!(win.as_str(), "windows-x86_64");
        assert!(!win.matches(&mac));
        assert!(win.matches(&Platform::parse("windows").unwrap()));

        assert!(Platform::parse("").is_err());
        assert!(Platform::parse("solaris").is_err());
        assert!(Platform::parse("linux-").is_err());
        assert!(Platform::parse("linux-x86-64").is_err());
    }
}
