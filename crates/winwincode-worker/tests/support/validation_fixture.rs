// SPDX-License-Identifier: Apache-2.0

pub(crate) fn with_python_executable(configuration: &str) -> String {
    let output = std::process::Command::new("/usr/bin/python3")
        .args(["-B", "-c", "import sys; print(sys.executable)"])
        .output()
        .expect("fixture Python executable should resolve");
    assert!(
        output.status.success(),
        "fixture Python executable failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let executable = String::from_utf8(output.stdout).expect("fixture Python path should be UTF-8");
    configuration.replace(
        "\"/usr/bin/python3\"",
        &serde_json::to_string(executable.trim()).expect("fixture Python path should encode"),
    )
}
