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
        command => Some(canonical_argv(command)),
    }
}

fn canonical_command(command: &str) -> Option<String> {
    let normalized = command.trim();
    if normalized.is_empty() {
        return None;
    }
    // Only literal, standalone invocations share argv identity. Expansions,
    // redirection and shell control flow retain their exact script identity.
    Some(literal_shell_argv(normalized).map_or_else(
        || format!("shell:{normalized}"),
        |arguments| canonical_argv(&arguments),
    ))
}

fn canonical_argv(arguments: &[String]) -> String {
    let mut identity = String::from("argv:");
    for argument in arguments {
        identity.push_str(&argument.len().to_string());
        identity.push(':');
        identity.push_str(argument);
    }
    identity
}

fn literal_shell_argv(command: &str) -> Option<Vec<String>> {
    let mut arguments = Vec::new();
    let mut argument = String::new();
    let mut quote = None;
    let mut started = false;
    let mut characters = command.chars();
    while let Some(character) = characters.next() {
        if matches!(character, '\0' | '\n' | '\r') {
            return None;
        }
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '$' | '`')
            | (
                None,
                '$' | '`' | '*' | '?' | '[' | ']' | '~' | ';' | '|' | '&' | '<' | '>' | '(' | ')'
                | '{' | '}' | '#',
            ) => return None,
            (Some('"'), '\\') => {
                let escaped = characters.next()?;
                if matches!(escaped, '\0' | '\n' | '\r') {
                    return None;
                }
                if !matches!(escaped, '$' | '`' | '"' | '\\') {
                    argument.push('\\');
                }
                argument.push(escaped);
            }
            (Some('\'' | '"'), _) => argument.push(character),
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (None, '\\') => {
                let escaped = characters.next()?;
                if matches!(escaped, '\0' | '\n' | '\r') {
                    return None;
                }
                argument.push(escaped);
                started = true;
            }
            (None, ' ' | '\t') => {
                if started {
                    arguments.push(std::mem::take(&mut argument));
                    started = false;
                }
            }
            (None, _) => {
                argument.push(character);
                started = true;
            }
            _ => return None,
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        arguments.push(argument);
    }
    let executable = arguments.first()?;
    if executable.is_empty()
        || executable.contains('=')
        || matches!(
            executable.as_str(),
            "!" | "if"
                | "then"
                | "else"
                | "fi"
                | "for"
                | "while"
                | "until"
                | "case"
                | "do"
                | "done"
                | "function"
                | "time"
        )
    {
        return None;
    }
    Some(arguments)
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
    fn literal_shell_quoting_does_not_change_verification_identity() {
        let method = "'/usr/bin/python3' '-I' '/scripts/smoke.py' '--verify-source' '.'";
        let observed = [
            "/bin/zsh",
            "-lc",
            "/usr/bin/python3 -I /scripts/smoke.py --verify-source .",
        ]
        .map(str::to_owned);
        assert_eq!(
            verification_method_digest(method),
            observed_verification_command_digest(&observed)
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
            verification_method_digest("printf 'a b'")
        );
        assert_ne!(
            verification_method_digest("printf a b"),
            verification_method_digest("printf 'a b'")
        );
        assert_eq!(
            verification_method_digest("printf ''"),
            observed_verification_command_digest(&["printf".to_owned(), String::new()])
        );
        assert_eq!(
            verification_method_digest("printf 'a'\\''b'"),
            verification_method_digest("printf \"a'b\"")
        );
    }

    #[test]
    fn shell_expansions_and_control_flow_do_not_match_literal_argv() {
        for script in [
            "printf $HOME",
            "printf ~",
            "printf *",
            "printf $(id)",
            "printf `id`",
            "printf a; true",
            "printf a || true",
            "printf a > output",
            "printf a\ntrue",
            "X=y printf a",
        ] {
            let argv = script
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert_ne!(
                verification_method_digest(script),
                observed_verification_command_digest(&argv),
                "{script}"
            );
        }
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
