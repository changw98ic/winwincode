// SPDX-License-Identifier: Apache-2.0
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

#[test]
fn apply_patch_aliases_accept_stdin_and_argument_patches() {
    let root = std::env::temp_dir().join(format!("helper-aliases-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for alias in ["apply_patch", "applypatch"] {
        for stdin in [true, false] {
            let filename = format!("{alias}-{stdin}");
            let patch =
                format!("*** Begin Patch\n*** Add File: {filename}\n+applied\n*** End Patch\n");
            let mut command = Command::new(env!("CARGO_BIN_EXE_winwincode-kernel-helper"));
            command
                .arg0(alias)
                .current_dir(&root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if !stdin {
                command.arg(&patch);
            }
            let mut child = command.spawn().unwrap();
            if stdin {
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(patch.as_bytes())
                    .unwrap();
            } else {
                drop(child.stdin.take());
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{alias} stdin={stdin}: {output:?}");
            assert_eq!(
                std::fs::read_to_string(root.join(filename)).unwrap(),
                "applied\n"
            );
        }
    }
    let handshake = Command::new(env!("CARGO_BIN_EXE_winwincode-kernel-helper"))
        .arg("--winwincode-helper-handshake")
        .output()
        .unwrap();
    assert!(handshake.status.success());
    assert!(
        String::from_utf8(handshake.stdout)
            .unwrap()
            .contains("winwincode-kernel-helper")
    );
    let host = Command::new(env!("CARGO_BIN_EXE_winwincode-kernel-helper"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(host.status.success(), "{host:?}");
    std::fs::remove_dir_all(root).unwrap();
}
