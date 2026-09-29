// SPDX-License-Identifier: Apache-2.0

//! Small, shared Git-diff checks used before and after Worker candidate upload.

use sha2::{Digest, Sha256};

/// Why a rework diff cannot be mapped to one exact source hunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitHunkOriginError {
    InvalidHeader,
    InvalidRange,
    NoContainingSourceHunk,
    AmbiguousSourceHunk,
}

/// Maps each replacement hunk digest to its containing source hunk digest.
///
/// Both inputs must be the exact per-path Git diff bytes from the retained
/// source candidate and the proposed replacement. Whole-hunk containment is
/// intentional: a broader Git context is rejected rather than widening scope.
///
/// # Errors
/// Returns an error for malformed text hunks or a replacement hunk that is not
/// contained by exactly one source hunk.
pub fn rework_hunk_origins(
    previous: &[u8],
    delta: &[u8],
) -> Result<Vec<(String, String)>, GitHunkOriginError> {
    fn sections(diff: &[u8]) -> Vec<&[u8]> {
        let starts = std::iter::once(0)
            .chain(diff.iter().enumerate().filter_map(|(index, byte)| {
                (*byte == b'\n' && index + 1 < diff.len()).then_some(index + 1)
            }))
            .filter(|start| diff[*start..].starts_with(b"@@ "))
            .collect::<Vec<_>>();
        if starts.is_empty() {
            return vec![diff];
        }
        starts
            .iter()
            .enumerate()
            .map(|(index, start)| {
                let end = starts.get(index + 1).copied().unwrap_or(diff.len());
                &diff[*start..end]
            })
            .collect()
    }

    fn range(hunk: &[u8], index: usize, sign: char) -> Result<(u64, u64), GitHunkOriginError> {
        let header = hunk.split(|byte| *byte == b'\n').next().unwrap_or_default();
        let field = std::str::from_utf8(header)
            .ok()
            .filter(|line| line.starts_with("@@ "))
            .and_then(|line| line.split_ascii_whitespace().nth(index))
            .and_then(|field| field.strip_prefix(sign))
            .ok_or(GitHunkOriginError::InvalidHeader)?;
        let (start, count) = field.split_once(',').unwrap_or((field, "1"));
        let start = start
            .parse::<u64>()
            .map_err(|_| GitHunkOriginError::InvalidRange)?;
        let count = count
            .parse::<u64>()
            .map_err(|_| GitHunkOriginError::InvalidRange)?;
        Ok((
            start,
            start
                .checked_add(count)
                .ok_or(GitHunkOriginError::InvalidRange)?,
        ))
    }

    let sources = sections(previous)
        .into_iter()
        .map(|hunk| Ok((format!("{:x}", Sha256::digest(hunk)), range(hunk, 2, '+')?)))
        .collect::<Result<Vec<_>, GitHunkOriginError>>()?;
    sections(delta)
        .into_iter()
        .map(|hunk| {
            let (start, end) = range(hunk, 1, '-')?;
            let mut matches = sources
                .iter()
                .filter(|(_, (left, right))| *left <= start && end <= *right);
            let source = matches
                .next()
                .ok_or(GitHunkOriginError::NoContainingSourceHunk)?;
            if matches.next().is_some() {
                return Err(GitHunkOriginError::AmbiguousSourceHunk);
            }
            Ok((format!("{:x}", Sha256::digest(hunk)), source.0.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{GitHunkOriginError, rework_hunk_origins};
    use sha2::Digest as _;

    #[test]
    fn accepts_a_replacement_inside_the_exact_source_hunk() {
        let source = b"@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let delta = b"@@ -1,3 +1,3 @@\n a\n-B\n+C\n c\n";
        let origins = rework_hunk_origins(source, delta).expect("contained hunk");
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].1, format!("{:x}", sha2::Sha256::digest(source)));
    }

    #[test]
    fn rejects_a_replacement_outside_the_source_hunk() {
        let source = b"@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let delta = b"@@ -8,3 +8,3 @@\n-x\n+y\n";
        assert_eq!(
            rework_hunk_origins(source, delta),
            Err(GitHunkOriginError::NoContainingSourceHunk)
        );
    }
}
