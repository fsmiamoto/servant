use std::process::Command;

const LIFECYCLE_COMMANDS: &[&str] = &[
    "install",
    "uninstall",
    "start",
    "stop",
    "restart",
    "status",
    "logs",
];

fn servant_bin() -> &'static str {
    env!("CARGO_BIN_EXE_servant")
}

#[test]
fn top_level_help_lists_service_but_not_lifecycle_commands() {
    let output = Command::new(servant_bin())
        .arg("--help")
        .output()
        .expect("run servant --help");

    assert!(output.status.success(), "status: {:?}", output.status);
    let stdout = String::from_utf8(output.stdout).expect("help is utf-8");

    assert!(stdout.contains("\n  service"), "{stdout}");
    for command in LIFECYCLE_COMMANDS {
        assert!(
            !stdout.contains(&format!("\n  {command}")),
            "top-level help should not list {command}:\n{stdout}"
        );
    }
}

#[test]
fn service_help_lists_lifecycle_commands() {
    let output = Command::new(servant_bin())
        .args(["service", "--help"])
        .output()
        .expect("run servant service --help");

    assert!(output.status.success(), "status: {:?}", output.status);
    let stdout = String::from_utf8(output.stdout).expect("help is utf-8");

    for command in LIFECYCLE_COMMANDS {
        assert!(
            stdout.contains(&format!("\n  {command}")),
            "service help should list {command}:\n{stdout}"
        );
    }
}

#[test]
fn old_top_level_lifecycle_help_exits_nonzero() {
    for command in LIFECYCLE_COMMANDS {
        let output = Command::new(servant_bin())
            .args([*command, "--help"])
            .output()
            .unwrap_or_else(|e| panic!("run servant {command} --help: {e}"));

        assert!(
            !output.status.success(),
            "servant {command} --help unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn service_status_json_unreachable_hint_uses_service_commands() {
    let home = tempfile::tempdir().expect("temp home");
    let output = Command::new(servant_bin())
        .args(["service", "status", "--json"])
        .env("HOME", home.path())
        .output()
        .expect("run servant service status --json");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    let value: serde_json::Value = serde_json::from_str(stderr.trim()).expect("json error");

    assert_eq!(value["code"], 2);
    let error = value["error"].as_str().expect("error string");
    assert!(error.contains("servant service start"), "{error}");
    assert!(error.contains("servant service install"), "{error}");
}
