// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};

use crate::Sha256Digest;

const DIGEST_DOMAIN: &[u8] = b"winwincode-verification-command-v1\0";

/// Returns the sealed identity of one approved verification command.
#[must_use]
pub fn verification_method_digest(method: &str) -> Option<Sha256Digest> {
    Some(command_digest(canonical_command(method)?.as_bytes()))
}

/// Returns the same sealed identity for an observed process invocation.
#[must_use]
pub fn observed_verification_command_digest(command: &[String]) -> Option<Sha256Digest> {
    Some(command_digest(
        canonical_observed_command(command)?.as_bytes(),
    ))
}

/// Classifies an observed command from its executable and leading arguments.
#[must_use]
pub fn observed_verification_command_is_test(command: &[String]) -> bool {
    let Some(tokens) = observed_command_tokens(command) else {
        return false;
    };
    let tokens = tokens
        .into_iter()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let tokens = tokens.as_slice();
    let tokens = match tokens {
        [corepack, rest @ ..] if corepack == "corepack" => rest,
        _ => tokens,
    };
    match tokens {
        [tool, subcommand, ..] if tool == "cargo" => {
            matches!(subcommand.as_str(), "test" | "nextest")
        }
        [tool, run, subcommand, ..]
            if matches!(tool.as_str(), "pnpm" | "npm" | "yarn" | "bun") && run == "run" =>
        {
            subcommand == "test"
        }
        [tool, subcommand, ..] if matches!(tool.as_str(), "pnpm" | "yarn" | "bun") => {
            subcommand == "test"
        }
        [tool, subcommand, ..] if tool == "npm" => subcommand == "test",
        [tool, ..] if matches!(tool.as_str(), "pytest" | "gradle") => true,
        [tool, module, runner, ..] if tool == "python" && module == "-m" => runner == "pytest",
        [tool, subcommand, ..] if matches!(tool.as_str(), "go" | "dotnet" | "swift") => {
            subcommand == "test"
        }
        [tool, subcommand, ..] if tool == "mvn" => subcommand == "test",
        _ => false,
    }
}

fn canonical_observed_command(command: &[String]) -> Option<String> {
    match command {
        [shell, flag, script]
            if matches!(shell.rsplit('/').next(), Some("sh" | "bash" | "zsh"))
                && matches!(flag.as_str(), "-c" | "-lc") =>
        {
            canonical_command(script)
        }
        [] => None,
        command
            if command.iter().all(|argument| {
                !argument.is_empty()
                    && !argument
                        .chars()
                        .any(|character| character.is_ascii_whitespace())
            }) =>
        {
            canonical_command(&command.join(" "))
        }
        _ => None,
    }
}

fn canonical_command(command: &str) -> Option<String> {
    let normalized = command.trim();
    (!normalized.is_empty()).then(|| normalized.to_owned())
}

fn observed_command_tokens(command: &[String]) -> Option<Vec<&str>> {
    match command {
        [shell, flag, script]
            if matches!(shell.rsplit('/').next(), Some("sh" | "bash" | "zsh"))
                && matches!(flag.as_str(), "-c" | "-lc") =>
        {
            let tokens = script.split_whitespace().collect::<Vec<_>>();
            (!tokens.is_empty()).then_some(tokens)
        }
        [] => None,
        command => Some(command.iter().map(String::as_str).collect()),
    }
}

fn command_digest(command: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(command);
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_shell_command_matches_approved_method() {
        let observed = [
            "/bin/zsh".to_owned(),
            "-lc".to_owned(),
            "  cargo test --workspace  ".to_owned(),
        ];
        assert_eq!(
            observed_verification_command_digest(&observed),
            verification_method_digest("cargo test --workspace")
        );
    }

    #[test]
    fn command_identity_preserves_semantically_relevant_whitespace_and_argv_boundaries() {
        assert_ne!(
            verification_method_digest("printf 'a b'"),
            verification_method_digest("printf 'a  b'")
        );
        assert_eq!(
            observed_verification_command_digest(&["printf".to_owned(), "a b".to_owned(),]),
            None
        );
    }

    #[test]
    fn mentioning_a_test_command_does_not_classify_an_unrelated_command_as_test() {
        assert!(!observed_verification_command_is_test(&[
            "printf".to_owned(),
            "npm test".to_owned(),
        ]));
        assert!(observed_verification_command_is_test(&[
            "corepack".to_owned(),
            "pnpm".to_owned(),
            "run".to_owned(),
            "test".to_owned(),
        ]));
    }
}
