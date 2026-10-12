// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfinedRoot {
    canonical_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfinementVerdict {
    Root,
    Inside { relative: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfinedPath {
    canonical_path: PathBuf,
    verdict: ConfinementVerdict,
}

impl ConfinedPath {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    #[must_use]
    pub fn verdict(&self) -> &ConfinementVerdict {
        &self.verdict
    }

    #[must_use]
    pub fn relative(&self) -> &Path {
        match &self.verdict {
            ConfinementVerdict::Root => Path::new(""),
            ConfinementVerdict::Inside { relative } => relative.as_path(),
        }
    }

    #[must_use]
    pub fn is_root(&self) -> bool {
        self.verdict == ConfinementVerdict::Root
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathConfinementError {
    NotAbsolute { input: String },
    LexicalEscape { input: String },
    NotCanonical { input: String, reason: String },
    Outside { candidate: String },
}

impl PathConfinementError {
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

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.canonical_root
    }

    pub fn confine(&self, candidate: &Path) -> Result<ConfinementVerdict, PathConfinementError> {
        require_absolute(candidate)?;
        require_lexically_canonical(candidate)?;
        let relative = candidate.strip_prefix(&self.canonical_root).map_err(|_| {
            PathConfinementError::Outside {
                candidate: candidate.to_string_lossy().into_owned(),
            }
        })?;
        require_resolves_to_self(candidate)?;
        Ok(if relative.as_os_str().is_empty() {
            ConfinementVerdict::Root
        } else {
            ConfinementVerdict::Inside {
                relative: relative.to_path_buf(),
            }
        })
    }

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

fn require_absolute(path: &Path) -> Result<(), PathConfinementError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(PathConfinementError::NotAbsolute {
            input: path.to_string_lossy().into_owned(),
        })
    }
}

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

        assert!(matches!(
            ConfinedRoot::new(Path::new("relative/path")),
            Err(PathConfinementError::NotAbsolute { .. })
        ));
        let escape = root_directory.join("..").join("elsewhere");
        assert!(matches!(
            ConfinedRoot::new(&escape),
            Err(PathConfinementError::LexicalEscape { .. })
        ));
        assert!(matches!(
            ConfinedRoot::new(&base.join("never")),
            Err(PathConfinementError::NotCanonical { .. })
        ));
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
            std::os::unix::fs::symlink(&outside_target, root_directory.join("jump"))
                .expect("jump symlink created");
            assert!(matches!(
                root.confine(&root_directory.join("jump")),
                Err(PathConfinementError::NotCanonical { .. })
            ));
            std::os::unix::fs::symlink(&child, root_directory.join("alias"))
                .expect("alias symlink created");
            assert!(matches!(
                root.confine(&root_directory.join("alias")),
                Err(PathConfinementError::NotCanonical { .. })
            ));
            assert!(matches!(
                root.confine(&child),
                Ok(ConfinementVerdict::Inside { .. })
            ));
        }

        let climb = root_directory.join("sub").join("..").join("file.txt");
        assert!(matches!(
            root.confine(&climb),
            Err(PathConfinementError::LexicalEscape { .. })
        ));
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

        assert!(matches!(
            root.canonicalize_within(&base.join("elsewhere")),
            Err(PathConfinementError::Outside { .. })
        ));
        assert!(matches!(
            root.canonicalize_within(&base.join("never")),
            Err(PathConfinementError::NotCanonical { .. })
        ));
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
