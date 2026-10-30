//! The application version and the project's bump rules.
//!
//! `Cargo.toml` is the single source of truth: `VERSION` is read from it at
//! compile time, and the `.deb` package version comes from the same field,
//! so nothing else in the tree may hardcode a version number.
//!
//! The numbering is not semver. Every shipped ticket bumps the version:
//! a small ticket adds 0.0.1, a major update adds 0.1.0 and resets the last
//! number, and the last two numbers never exceed 9 — reaching 10 rolls over
//! into the number before it (`0.0.9` → `0.1.0`, `0.9.9` → `1.0.0`). The
//! first number has nothing to roll into and simply keeps counting.
//! `examples/bump_version.rs` applies these rules to `Cargo.toml`.

use anyhow::{bail, Context, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The read-only version entry shown beneath Settings in the main menu.
pub fn display_label() -> String {
    format!("Version {VERSION}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bump {
    /// A small ticket: +0.0.1.
    Small,
    /// A major update: +0.1.0, last number reset to 0.
    Major,
}

impl Bump {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "small" => Some(Self::Small),
            "major" => Some(Self::Major),
            _ => None,
        }
    }
}

/// Apply `kind` to `version` (`X.Y.Z`), rolling any number that reaches 10
/// over into the number before it.
pub fn bump(version: &str, kind: Bump) -> Result<String> {
    let parts: Vec<&str> = version.split('.').collect();
    let [x, y, z] = parts.as_slice() else {
        bail!("version {version:?} is not in X.Y.Z form");
    };
    let parse = |s: &str| -> Result<u32> {
        s.parse()
            .with_context(|| format!("version {version:?} has a non-numeric part {s:?}"))
    };
    let (mut x, mut y, mut z) = (parse(x)?, parse(y)?, parse(z)?);
    if y > 9 || z > 9 {
        bail!("version {version:?} breaks the numbering rules: the last two numbers must be 0-9");
    }

    match kind {
        Bump::Small => z += 1,
        Bump::Major => {
            y += 1;
            z = 0;
        }
    }
    if z == 10 {
        z = 0;
        y += 1;
    }
    if y == 10 {
        y = 0;
        x += 1;
    }
    Ok(format!("{x}.{y}.{z}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_bump_adds_to_the_last_number() {
        assert_eq!(bump("0.1.0", Bump::Small).unwrap(), "0.1.1");
    }

    #[test]
    fn small_bump_rolls_the_last_number_into_the_middle() {
        assert_eq!(bump("0.0.9", Bump::Small).unwrap(), "0.1.0");
    }

    #[test]
    fn small_bump_rolls_over_twice() {
        assert_eq!(bump("0.9.9", Bump::Small).unwrap(), "1.0.0");
    }

    #[test]
    fn major_bump_adds_to_the_middle_number_and_resets_the_last() {
        assert_eq!(bump("0.1.4", Bump::Major).unwrap(), "0.2.0");
    }

    #[test]
    fn major_bump_rolls_the_middle_number_into_the_first() {
        assert_eq!(bump("0.9.4", Bump::Major).unwrap(), "1.0.0");
    }

    #[test]
    fn first_number_keeps_counting_past_nine() {
        assert_eq!(bump("9.9.9", Bump::Small).unwrap(), "10.0.0");
    }

    #[test]
    fn malformed_versions_are_rejected() {
        for bad in [
            "1.0",
            "1.0.0.0",
            "1.a.0",
            "",
            "0.10.0",
            "0.0.12",
            "1.0.0-beta",
        ] {
            assert!(
                bump(bad, Bump::Small).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn bump_kinds_parse_from_their_names() {
        assert_eq!(Bump::parse("small"), Some(Bump::Small));
        assert_eq!(Bump::parse("major"), Some(Bump::Major));
        assert_eq!(Bump::parse("patch"), None);
    }

    #[test]
    fn crate_version_follows_the_numbering_rules() {
        // The shipped version must itself be bumpable, or the next bump fails.
        bump(VERSION, Bump::Small).unwrap();
    }

    #[test]
    fn display_label_shows_the_crate_version() {
        assert_eq!(
            display_label(),
            format!("Version {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
