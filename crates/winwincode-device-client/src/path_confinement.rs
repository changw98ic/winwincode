// SPDX-License-Identifier: Apache-2.0

//! Fail-closed path confinement for repository bindings: containment of
//! canonical paths within an authorized binding root, at component
//! boundaries.
//!
//! This is the shared primitive the Worker launch path (plan 13.5, the
//! `每次 Worker Launch 前必须重新 canonicalize` rule) and any future
//! candidate/apply path reuse to prove that a path they are about to touch
//! is still inside the root a binding authorized — never a sibling that
//! merely shares a string prefix, never a symlink spelling that resolves
//! elsewhere.
//!
//! The API is deliberate about what it reads:
//!
//! - [`ConfinedRoot::confine`] decides containment with pure component
//!   logic first, so a candidate outside the root is refused **without
//!   touching the filesystem at all**. Only a lexically-inside candidate is
//!   canonicalization-checked (a read that therefore stays inside the
//!   authorized root), and an exact self-canonicalization proves the
//!   spelling carries no symlink or aliasing layer.
//! - [`ConfinedRoot::canonicalize_within`] takes the one raw requested
//!   entry, resolves it (the requested entry itself is the only path read,
//!   and only to resolve it), and refuses it immediately when it falls
//!   outside the root.
//!
//! Contract: every path handed to [`ConfinedRoot::confine`] must already be
//! canonical (the registry check chain canonicalizes before anything else).
//! Non-canonical spellings — symlink entries, `..` climbs, aliased
//! directories — are refused, never silently resolved: fail closed.
//!
//! Local-data boundary: error details carry absolute paths because this API
//! is local-only scaffolding; its values must never be serialized into a
//! server-bound frame.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// A binding's authorized root, constructed only from a proven-canonical
/// absolute directory. The registry's stored canonical path is the intended
/// source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfinedRoot {
    canonical_root: PathBuf,
}

/// The containment verdict for one canonical path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfinementVerdict {
    /// The candidate is the authorized root itself.
    Root,
    /// The candidate lies strictly inside the root; `relative` is the
    /// non-empty suffix at component boundaries, with no leading separator.
    Inside {
        /// The suffix of the candidate relative to the root.
        relative: PathBuf,
    },
}

/// One path that was resolved and then proven confined.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfinedPath {
    canonical_path: PathBuf,
    verdict: ConfinementVerdict,
}

impl ConfinedPath {
    /// The canonical path, proven to be inside (or equal to) the root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    /// The containment verdict.
    #[must_use]
    pub fn verdict(&self) -> &ConfinementVerdict {
        &self.verdict
    }

    /// The suffix relative to the root; empty for the root itself.
    #[must_use]
    pub fn relative(&self) -> &Path {
        match &self.verdict {
            ConfinementVerdict::Root => Path::new(""),
            ConfinementVerdict::Inside { relative } => relative.as_path(),
        }
    }

    /// Whether the confined path is the root itself.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.verdict == ConfinementVerdict::Root
    }
}

/// Why confinement refused a path. Local-only: the details carry absolute
/// paths and must never reach a server-bound frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathConfinementError {
    /// The input is not an absolute path.
    NotAbsolute {
        /// The refused input.
        input: String,
    },
    /// The input contains a `..` climb (or a leading `.`), so it is not a
    /// canonical spelling.
    LexicalEscape {
        /// The refused input.
        input: String,
    },
    /// The input is not the canonical spelling of an existing path (a
    /// symlink entry, an aliased directory, or an unresolvable path).
    NotCanonical {
        /// The refused input.
        input: String,
        /// Why the canonicality proof failed.
        reason: String,
    },
    /// The candidate lies outside the authorized root at a component
    /// boundary. Refused purely lexically, without touching the filesystem.
    Outside {
        /// The refused candidate.
        candidate: String,
    },
}

impl PathConfinementError {
    /// The refused input as given.
    #[must_use]
    pub fn input(&self) -> &str {
        match self {
            Self::NotAbsolute { input }
            | Self::LexicalEscape { input }
            | Self::NotCanonical { input, .. }
            | Self::Outside { candidate: input } => input,
        }
    }
}

impl fmt::Display for PathConfinementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAbsolute { input } => {
                write!(
                    formatter,
                    "path confinement requires an absolute path: {input}"
                )
            }
            Self::LexicalEscape { input } => {
                write!(
                    formatter,
                    "path confinement refuses dot-dot path spellings: {input}"
                )
            }
            Self::NotCanonical { input, reason } => {
                write!(
                    formatter,
                    "path confinement requires a canonical path: {input} ({reason})"
                )
            }
            Self::Outside { candidate } => {
                write!(
                    formatter,
                    "the path is outside the authorized root: {candidate}"
                )
            }
        }
    }
}

impl Error for PathConfinementError {}

impl ConfinedRoot {
    /// Authorizes one root. The path must be absolute, free of `..`/`.`
    /// spellings, and prove canonical: it must resolve to itself exactly,
    /// which refuses symlink spellings and aliased directories and proves
    /// the root exists.
    ///
    /// # Errors
    ///
    /// Returns [`PathConfinementError`] for every non-canonical input.
    pub fn new(canonical_root: &Path) -> Result<Self, PathConfinementError> {
        let input = canonical_root.to_string_lossy().into_owned();
        require_absolute(canonical_root)?;
        require_lexically_canonical(canonical_root)?;
        match fs::canonicalize(canonical_root) {
            Ok(resolved) if resolved == canonical_root => Ok(Self {
                canonical_root: canonical_root.to_path_buf(),
            }),
            Ok(resolved) => Err(PathConfinementError::NotCanonical {
                input,
                reason: format!("it resolves to {}", resolved.to_string_lossy()),
            }),
            Err(error) => Err(PathConfinementError::NotCanonical {
                input,
                reason: format!("it cannot be resolved ({error})"),
            }),
        }
    }

    /// The authorized root as given (canonical by construction).
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.canonical_root
    }

    /// Proves one already-canonical candidate is the root or lies inside it,
    /// at component boundaries.
    ///
    /// A candidate outside the root is refused without touching the
    /// filesystem. A lexically-inside candidate must also prove canonical
    /// (it resolves to itself exactly), which fails closed on symlink
    /// entries — including one that jumps outside the root or to another
    /// spelling inside it.
    ///
    /// # Errors
    ///
    /// Returns [`PathConfinementError`] for every refused candidate.
    pub fn confine(&self, candidate: &Path) -> Result<ConfinementVerdict, PathConfinementError> {
        require_absolute(candidate)?;
        require_lexically_canonical(candidate)?;
        // Pure component containment first: an outside candidate is refused
        // with zero filesystem reads.
        let relative = candidate.strip_prefix(&self.canonical_root).map_err(|_| {
            PathConfinementError::Outside {
                candidate: candidate.to_string_lossy().into_owned(),
            }
        })?;
        // The candidate is lexically inside the root, so this metadata read
        // stays inside the authorized root. An exact self-canonicalization
        // proves the spelling carries no symlink or aliasing layer.
        require_resolves_to_self(candidate)?;
        Ok(if relative.as_os_str().is_empty() {
            ConfinementVerdict::Root
        } else {
            ConfinementVerdict::Inside {
                relative: relative.to_path_buf(),
            }
        })
    }

    /// Resolves one raw requested entry and proves it confined: the
    /// single entry point a launch path needs to re-canonicalize a stored or
    /// requested path and immediately confirm it still belongs to the
    /// binding's root.
    ///
    /// The requested entry itself is the only path read here — and only to
    /// resolve it; containment is decided immediately after, before any
    /// other use.
    ///
    /// # Errors
    ///
    /// Returns [`PathConfinementError`] when the entry is missing,
    /// non-canonical in spelling, or outside the root.
    pub fn canonicalize_within(
        &self,
        requested: &Path,
    ) -> Result<ConfinedPath, PathConfinementError> {
        require_absolute(requested)?;
        require_lexically_canonical(requested)?;
        let canonical_path =
            fs::canonicalize(requested).map_err(|error| PathConfinementError::NotCanonical {
                input: requested.to_string_lossy().into_owned(),
                reason: format!("it cannot be resolved ({error})"),
            })?;
        let verdict = self.confine(&canonical_path)?;
        Ok(ConfinedPath {
            canonical_path,
            verdict,
        })
    }
}

/// Refuses relative inputs; confinement decisions are made on absolute
/// paths only.
fn require_absolute(path: &Path) -> Result<(), PathConfinementError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(PathConfinementError::NotAbsolute {
            input: path.to_string_lossy().into_owned(),
        })
    }
}

/// Refuses `..` climbs (and a leading `.`): canonical spellings never carry
/// them. Mid-path `.` components are normalized away by [`Component`], so
/// the canonicality proof below covers the rest.
fn require_lexically_canonical(path: &Path) -> Result<(), PathConfinementError> {
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        Err(PathConfinementError::LexicalEscape {
            input: path.to_string_lossy().into_owned(),
        })
    } else {
        Ok(())
    }
}

/// Proves a path is the canonical spelling of an existing path: it resolves
/// to itself exactly. A symlink entry — even one pointing at the same
/// directory through a different spelling — is refused.
fn require_resolves_to_self(path: &Path) -> Result<(), PathConfinementError> {
    match fs::canonicalize(path) {
        Ok(resolved) if resolved == path => Ok(()),
        Ok(resolved) => Err(PathConfinementError::NotCanonical {
            input: path.to_string_lossy().into_owned(),
            reason: format!("it resolves to {}", resolved.to_string_lossy()),
        }),
        Err(error) => Err(PathConfinementError::NotCanonical {
            input: path.to_string_lossy().into_owned(),
            reason: format!("it cannot be resolved ({error})"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMPORARY_BASE: AtomicU64 = AtomicU64::new(1);

    /// One canonical temporary base directory (canonicalized because the
    /// platform temp path itself may be a symlink spelling).
    fn temporary_base(name: &str) -> PathBuf {
        let suffix = NEXT_TEMPORARY_BASE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "winwincode-path-confinement-{name}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("temporary base directory");
        fs::canonicalize(&base).expect("canonical temporary base")
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    fn confined_root(root_directory: &Path) -> ConfinedRoot {
        ConfinedRoot::new(root_directory).expect("canonical root")
    }

    #[test]
    fn confined_root_accepts_only_proven_canonical_roots() {
        let base = temporary_base("root-shapes");
        let root_directory = base.join("repo");
        fs::create_dir_all(&root_directory).expect("root directory");

        let root = confined_root(&root_directory);
        assert_eq!(root.as_path(), root_directory);

        // Relative input.
        assert!(matches!(
            ConfinedRoot::new(Path::new("relative/path")),
            Err(PathConfinementError::NotAbsolute { .. })
        ));
        // Dot-dot climb.
        let escape = root_directory.join("..").join("elsewhere");
        assert!(matches!(
            ConfinedRoot::new(&escape),
            Err(PathConfinementError::LexicalEscape { .. })
        ));
        // Missing path.
        assert!(matches!(
            ConfinedRoot::new(&base.join("never")),
            Err(PathConfinementError::NotCanonical { .. })
        ));
        // Symlink spelling of a real directory.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&root_directory, base.join("link"))
                .expect("symlink created");
            let error =
                ConfinedRoot::new(&base.join("link")).expect_err("symlink spelling is refused");
            assert!(matches!(error, PathConfinementError::NotCanonical { .. }));
            assert!(error.input().ends_with("link"));
        }

        cleanup(&base);
    }

    #[test]
    fn confine_accepts_the_root_and_reports_child_suffixes() {
        let base = temporary_base("confine-inside");
        let root_directory = base.join("repo");
        let child = root_directory.join("sub").join("deep");
        fs::create_dir_all(&child).expect("child directories");
        fs::write(root_directory.join("file.txt"), b"content").expect("child file");
        let root = confined_root(&root_directory);

        assert!(matches!(
            root.confine(&root_directory),
            Ok(ConfinementVerdict::Root)
        ));
        let verdict = root.confine(&child).expect("nested child is confined");
        assert_eq!(
            verdict,
            ConfinementVerdict::Inside {
                relative: PathBuf::from("sub/deep"),
            }
        );
        let verdict = root
            .confine(&root_directory.join("file.txt"))
            .expect("child file is confined");
        assert_eq!(
            verdict,
            ConfinementVerdict::Inside {
                relative: PathBuf::from("file.txt"),
            }
        );

        cleanup(&base);
    }

    #[test]
    fn confine_refuses_sibling_prefixes_without_touching_the_filesystem() {
        let base = temporary_base("confine-siblings");
        let root_directory = base.join("repo");
        fs::create_dir_all(&root_directory).expect("root directory");
        // String prefixes, not component prefixes: a naive prefix check
        // would accept both.
        fs::create_dir_all(base.join("repository")).expect("sibling one");
        fs::create_dir_all(base.join("repo-twin")).expect("sibling two");
        let root = confined_root(&root_directory);

        assert!(matches!(
            root.confine(&base.join("repository")),
            Err(PathConfinementError::Outside { .. })
        ));
        assert!(matches!(
            root.confine(&base.join("repo-twin")),
            Err(PathConfinementError::Outside { .. })
        ));
        // An outside candidate that does not exist at all is still refused
        // as `Outside` — proof the lexical containment decision precedes and
        // replaces any filesystem read outside the root.
        assert!(matches!(
            root.confine(&base.join("repository").join("never-created")),
            Err(PathConfinementError::Outside { .. })
        ));

        cleanup(&base);
    }

    #[test]
    fn confine_fails_closed_on_noncanonical_spellings_inside_the_root() {
        let base = temporary_base("confine-symlinks");
        let root_directory = base.join("repo");
        let child = root_directory.join("sub");
        fs::create_dir_all(&child).expect("child directory");
        let outside_target = base.join("outside");
        fs::create_dir_all(&outside_target).expect("outside directory");
        let root = confined_root(&root_directory);

        #[cfg(unix)]
        {
            // A symlink inside the root that jumps outside the root: the
            // lexical check passes, the canonicality proof refuses — never
            // a confinement success.
            std::os::unix::fs::symlink(&outside_target, root_directory.join("jump"))
                .expect("jump symlink created");
            assert!(matches!(
                root.confine(&root_directory.join("jump")),
                Err(PathConfinementError::NotCanonical { .. })
            ));
            // A symlink to an inside target with a different spelling is
            // equally refused: pass canonical spellings.
            std::os::unix::fs::symlink(&child, root_directory.join("alias"))
                .expect("alias symlink created");
            assert!(matches!(
                root.confine(&root_directory.join("alias")),
                Err(PathConfinementError::NotCanonical { .. })
            ));
            // The canonical spelling of the same target stays confined.
            assert!(matches!(
                root.confine(&child),
                Ok(ConfinementVerdict::Inside { .. })
            ));
        }

        // A dot-dot climb is refused lexically, before any read.
        let climb = root_directory.join("sub").join("..").join("file.txt");
        assert!(matches!(
            root.confine(&climb),
            Err(PathConfinementError::LexicalEscape { .. })
        ));
        // A relative candidate is refused outright.
        assert!(matches!(
            root.confine(Path::new("sub")),
            Err(PathConfinementError::NotAbsolute { .. })
        ));

        cleanup(&base);
    }

    #[test]
    fn canonicalize_within_resolves_and_confines_the_requested_entry() {
        let base = temporary_base("canonicalize-within");
        let root_directory = base.join("repo");
        let child = root_directory.join("sub");
        fs::create_dir_all(&child).expect("child directory");
        fs::create_dir_all(base.join("elsewhere")).expect("outside directory");
        let root = confined_root(&root_directory);

        let confined = root
            .canonicalize_within(&child)
            .expect("inside entry is confined");
        assert_eq!(confined.path(), child);
        assert!(matches!(
            confined.verdict(),
            ConfinementVerdict::Inside { .. }
        ));
        assert_eq!(confined.relative(), Path::new("sub"));
        assert!(!confined.is_root());

        let root_entry = root
            .canonicalize_within(&root_directory)
            .expect("the root itself is confined");
        assert!(root_entry.is_root());
        assert_eq!(root_entry.relative(), Path::new(""));

        // Outside requested entries are refused after resolution.
        assert!(matches!(
            root.canonicalize_within(&base.join("elsewhere")),
            Err(PathConfinementError::Outside { .. })
        ));
        // Missing entries are refused as unresolvable.
        assert!(matches!(
            root.canonicalize_within(&base.join("never")),
            Err(PathConfinementError::NotCanonical { .. })
        ));
        // Relative entries are refused outright.
        assert!(matches!(
            root.canonicalize_within(Path::new("sub")),
            Err(PathConfinementError::NotAbsolute { .. })
        ));

        cleanup(&base);
    }

    #[test]
    fn confinement_errors_are_stable_and_self_describing() {
        let outside = PathConfinementError::Outside {
            candidate: "/elsewhere".to_owned(),
        };
        assert_eq!(
            outside.to_string(),
            "the path is outside the authorized root: /elsewhere"
        );
        assert_eq!(outside.input(), "/elsewhere");

        let lexical = PathConfinementError::LexicalEscape {
            input: "/repo/../secret".to_owned(),
        };
        assert_eq!(
            lexical.to_string(),
            "path confinement refuses dot-dot path spellings: /repo/../secret"
        );

        let relative = PathConfinementError::NotAbsolute {
            input: "sub".to_owned(),
        };
        assert_eq!(
            relative.to_string(),
            "path confinement requires an absolute path: sub"
        );

        let aliased = PathConfinementError::NotCanonical {
            input: "/repo/alias".to_owned(),
            reason: "it resolves to /repo/sub".to_owned(),
        };
        assert_eq!(
            aliased.to_string(),
            "path confinement requires a canonical path: /repo/alias (it resolves to /repo/sub)"
        );
    }
}
