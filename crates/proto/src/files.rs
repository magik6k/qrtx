//! The `send`/`recv` transfer format, used between two computers and between
//! a computer and the phone.
//!
//! On the tunnel stream (after the [`crate::HANDSHAKE`]):
//! 1. sender → receiver: a [`Manifest`] frame;
//! 2. receiver → sender: [`Reply::Accept`] or [`Reply::Reject`];
//! 3. sender → receiver: the contents of every file entry, back to back, in
//!    manifest order (exactly `size` bytes each), then the stream is finished;
//! 4. receiver → sender: [`Reply::Done`] once everything is on disk.

use std::collections::HashSet;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// Manifests of big directories don't fit the usual frame limit.
pub const MAX_MANIFEST: usize = 16 << 20;
const MAX_PATH: usize = 4096;

/// One file or (possibly empty) directory, with a `/`-separated relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dir: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub exec: bool,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Entry {
    pub fn file(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
            dir: false,
            exec: false,
        }
    }

    pub fn dir(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            size: 0,
            dir: true,
            exec: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().filter(|e| !e.dir).map(|e| e.size).sum()
    }

    pub fn file_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.dir).count()
    }

    /// The names at the top level, in order (`photos`, `notes.txt`, …).
    pub fn top_level(&self) -> Vec<(&str, bool)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for e in &self.entries {
            let (first, rest) = e
                .path
                .split_once('/')
                .map_or((e.path.as_str(), None), |(a, b)| (a, Some(b)));
            if seen.insert(first) {
                out.push((first, e.dir || rest.is_some()));
            }
        }
        out
    }

    /// Everything a receiver must check before touching the disk.
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.entries.is_empty(), "nothing to send");
        let mut paths = HashSet::new();
        let mut files = HashSet::new();
        for e in &self.entries {
            check_path(&e.path)?;
            ensure!(!e.dir || e.size == 0, "directory {} has a size", e.path);
            ensure!(paths.insert(e.path.as_str()), "{} is listed twice", e.path);
            if !e.dir {
                files.insert(e.path.as_str());
            }
        }
        // a file can't also be a parent directory of another entry
        for e in &self.entries {
            let mut parent = e.path.as_str();
            while let Some((p, _)) = parent.rsplit_once('/') {
                ensure!(!files.contains(p), "{} is both a file and a directory", p);
                parent = p;
            }
        }
        Ok(())
    }

    /// Just the names: `photos/`, `a.jpg, b.jpg, c.jpg, +2 more`.
    pub fn label(&self) -> String {
        let top = self.top_level();
        let mut names: Vec<String> = top
            .iter()
            .take(3)
            .map(|(n, dir)| {
                if *dir {
                    format!("{n}/")
                } else {
                    (*n).to_owned()
                }
            })
            .collect();
        if top.len() > 3 {
            names.push(format!("+{} more", top.len() - 3));
        }
        names.join(", ")
    }

    /// Short human description: `photos/ (123 files, 2.3 GiB)`, `a.jpg (1.2 MiB)`,
    /// `a.jpg, b.jpg, c.jpg, +2 more (5 files, 12.0 MiB)`.
    pub fn summary(&self) -> String {
        let top = self.top_level();
        let names = self.label();
        let total = human_bytes(self.total_bytes());
        match (self.file_count(), top.as_slice()) {
            (1, [(_, false)]) => format!("{names} ({total})"),
            (1, _) => format!("{names} (1 file, {total})"),
            (n, _) => format!("{names} ({n} files, {total})"),
        }
    }
}

/// A safe relative path: `/`-separated, no empty, `.` or `..` components, no
/// backslashes, no leading slash, no NUL.
pub fn check_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "empty path");
    ensure!(path.len() <= MAX_PATH, "path too long");
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        bail!("unsafe path {path:?}");
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            bail!("unsafe path {path:?}");
        }
    }
    Ok(())
}

/// Receiver → sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case")]
pub enum Reply {
    Accept,
    Reject { message: String },
    Done { bytes: u64 },
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in ["a", "a/b", "photos/IMG 0001.jpg", ".hidden", "a/.b/c..d"] {
            check_path(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            ".",
            "a//b",
            "a/",
            "a\\b",
            "./a",
            "a\0b",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn validate() {
        let m = Manifest {
            entries: vec![Entry::dir("d"), Entry::file("d/a", 3), Entry::file("b", 1)],
        };
        m.validate().unwrap();
        assert_eq!(m.total_bytes(), 4);
        assert_eq!(m.file_count(), 2);
        assert!(Manifest::default().validate().is_err());
        let dup = Manifest {
            entries: vec![Entry::file("a", 1), Entry::file("a", 1)],
        };
        assert!(dup.validate().is_err());
        let clash = Manifest {
            entries: vec![Entry::file("a", 1), Entry::file("a/b", 1)],
        };
        assert!(clash.validate().is_err());
        let sized_dir = Manifest {
            entries: vec![Entry {
                size: 5,
                ..Entry::dir("d")
            }],
        };
        assert!(sized_dir.validate().is_err());
    }

    #[test]
    fn summaries() {
        let one = Manifest {
            entries: vec![Entry::file("backup.tar", 2048)],
        };
        assert_eq!(one.summary(), "backup.tar (2.0 KiB)");
        let dir = Manifest {
            entries: vec![
                Entry::dir("photos"),
                Entry::file("photos/a.jpg", 10),
                Entry::file("photos/b.jpg", 10),
            ],
        };
        assert_eq!(dir.summary(), "photos/ (2 files, 20 B)");
        let many = Manifest {
            entries: (0..5)
                .map(|i| Entry::file(format!("{i}.jpg"), 1024))
                .collect(),
        };
        assert_eq!(
            many.summary(),
            "0.jpg, 1.jpg, 2.jpg, +2 more (5 files, 5.0 KiB)"
        );
    }

    #[test]
    fn wire_format() {
        let m = Manifest {
            entries: vec![
                Entry::dir("d"),
                Entry {
                    exec: true,
                    ..Entry::file("d/run", 7)
                },
            ],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"entries":[{"path":"d","dir":true},{"path":"d/run","size":7,"exec":true}]}"#
        );
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
        assert_eq!(
            serde_json::to_string(&Reply::Done { bytes: 7 }).unwrap(),
            r#"{"t":"done","bytes":7}"#
        );
    }
}
