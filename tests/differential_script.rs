use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

fn write_executable(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write fake executable");
    let mut permissions = std::fs::metadata(path)
        .expect("fake executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make fake executable executable");
}

fn fake_tool_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("fake tool directory");
    write_executable(
        &directory.path().join("docker"),
        r#"#!/bin/sh
if [ "${1:-}" = compose ]; then exit 1; fi
printf '%s docker %s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$*" >> "$LIFECYCLE_LOG"
exit 0
"#,
    );
    write_executable(
        &directory.path().join("docker-compose"),
        r#"#!/bin/sh
printf '%s compose %s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$*" >> "$LIFECYCLE_LOG"
case " $* " in
  *" up "*)
    if [ "${FAIL_COMPOSE_UP:-0}" = 1 ]; then exit 23; fi
    ;;
  *" port redis 6379 "*) printf '127.0.0.1:43123\n' ;;
esac
exit 0
"#,
    );
    write_executable(
        &directory.path().join("cargo"),
        r#"#!/bin/sh
printf '%s cargo %s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$*" >> "$LIFECYCLE_LOG"
sleep "${FAKE_CARGO_SLEEP:-0}"
exit 0
"#,
    );
    directory
}

fn script_command(tools: &tempfile::TempDir, log: &std::path::Path) -> Command {
    let mut command = Command::new("bash");
    let path = format!(
        "{}:{}",
        tools.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    command
        .arg("scripts/test-differential.sh")
        .env("PATH", path)
        .env("LIFECYCLE_LOG", log)
        .env("DIFFERENTIAL_SKIP_RUNTIME_PROBE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn differential_script_cleans_partial_compose_failure() {
    let tools = fake_tool_directory();
    let log = tools.path().join("lifecycle.log");
    let status = script_command(&tools, &log)
        .env("FAIL_COMPOSE_UP", "1")
        .status()
        .expect("run differential script");
    assert_eq!(status.code(), Some(23));
    let log = std::fs::read_to_string(log).expect("read lifecycle log");
    let up = log.find(" up ").expect("compose up was attempted");
    let down = log.find(" down ").expect("cleanup down was attempted");
    assert!(
        down > up,
        "cleanup must run after the injected partial failure"
    );
}

#[test]
fn concurrent_differential_scripts_use_unique_projects_and_cleanup_each() {
    let tools = fake_tool_directory();
    let log = tools.path().join("concurrent.log");
    let mut first = script_command(&tools, &log)
        .env("FAKE_CARGO_SLEEP", "0.1")
        .spawn()
        .expect("spawn first differential script");
    let mut second = script_command(&tools, &log)
        .env("FAKE_CARGO_SLEEP", "0.1")
        .spawn()
        .expect("spawn second differential script");
    assert!(first.wait().expect("first script").success());
    assert!(second.wait().expect("second script").success());

    let log = std::fs::read_to_string(log).expect("read concurrent lifecycle log");
    let projects: BTreeSet<_> = log
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(project, _)| project.to_owned()))
        .filter(|project| project.starts_with("chp-diff-"))
        .collect();
    assert_eq!(
        projects.len(),
        2,
        "concurrent runs shared a Compose project: {log}"
    );
    for project in projects {
        assert!(
            log.lines()
                .any(|line| line.starts_with(&project) && line.contains(" down ")),
            "project {project} was not cleaned: {log}"
        );
    }
}

#[test]
fn just_recipe_executes_the_standalone_differential_script() {
    let justfile = std::fs::read_to_string("justfile").expect("read justfile");
    assert!(justfile.contains("./scripts/test-differential.sh"));
}
