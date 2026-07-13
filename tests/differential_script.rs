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
if [ "${1:-}" = ps ]; then
  if [ -s "${CARGO_PGID_FILE:-}" ]; then
    pgid="$(cat "$CARGO_PGID_FILE")"
    if kill -0 "-$pgid" 2>/dev/null; then
      printf '%s group-live-at-container-scan %s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$pgid" >> "$LIFECYCLE_LOG"
    fi
  fi
  case " $* " in
    *" --filter label=io.openrusty.chp-differential.run=${COMPOSE_PROJECT_NAME:-missing} "*)
      printf '%s-oracle-container\n' "${COMPOSE_PROJECT_NAME:-missing}"
      ;;
    *" --filter ancestor="*)
      printf '%s ancestor-filter-used\n' "${COMPOSE_PROJECT_NAME:-missing}" >> "$LIFECYCLE_LOG"
      printf 'shared-image-first-container\nshared-image-second-container\n'
      ;;
  esac
  exit 0
fi
case " $* " in
  *" image rm "*)
    if [ "${FAIL_IMAGE_RM:-0}" = 1 ]; then exit 26; fi
    ;;
  *" run "*)
    if [ "${FAIL_RUNTIME_PROBE:-0}" = 1 ]; then exit 24; fi
    if [ "${INTERRUPT_STAGE:-}" = runtime-probe ]; then sleep "${FAKE_STAGE_DELAY:-0}"; fi
    printf '%s\n' '{"node":"v20.20.2","package":"configurable-http-proxy@5.3.0","source":"/opt/chp-5.3.0"}'
    ;;
esac
exit 0
"#,
    );
    write_executable(
        &directory.path().join("docker-compose"),
        r#"#!/bin/sh
printf '%s compose %s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$*" >> "$LIFECYCLE_LOG"
case " $* " in
  *" build chp "*)
    if [ "${FAIL_BUILD:-0}" = 1 ]; then exit 21; fi
    if [ "${INTERRUPT_STAGE:-}" = build ]; then sleep "${FAKE_STAGE_DELAY:-0}"; fi
    ;;
  *" up "*)
    if [ "${FAIL_COMPOSE_UP:-0}" = 1 ]; then exit 23; fi
    if [ "${INTERRUPT_STAGE:-}" = up ]; then sleep "${FAKE_STAGE_DELAY:-0}"; fi
    ;;
  *" port redis 6379 "*) printf '127.0.0.1:43123\n' ;;
esac
exit 0
"#,
    );
    write_executable(
        &directory.path().join("cargo"),
        r#"#!/usr/bin/env bash
printf '%s cargo %s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$*" >> "$LIFECYCLE_LOG"
if [ "${FAKE_CARGO_LEADER_EXIT_TREE:-0}" = 1 ]; then
  (
    trap '' TERM
    sleep "${FAKE_LATE_CONTAINER_DELAY:-0.5}"
    printf '%s late-container-created\n' "${COMPOSE_PROJECT_NAME:-missing}" >> "$LIFECYCLE_LOG"
  ) &
  grandchild=$!
  disown "$grandchild"
  pgid="$(ps -o pgid= -p $$ | tr -d ' ')"
  printf '%s\n' "$pgid" > "$CARGO_PGID_FILE"
  printf '%s cargo-leader-exit leader=%s grandchild=%s pgid=%s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$$" "$grandchild" "$pgid" >> "$LIFECYCLE_LOG"
  exit 0
fi
if [ "${FAKE_CARGO_TREE:-0}" = 1 ]; then
  trap '' TERM
  (
    trap '' TERM
    sleep "${FAKE_LATE_CONTAINER_DELAY:-0.5}"
    printf '%s late-container-created\n' "${COMPOSE_PROJECT_NAME:-missing}" >> "$LIFECYCLE_LOG"
  ) &
  grandchild=$!
  pgid="$(ps -o pgid= -p $$ | tr -d ' ')"
  printf '%s\n' "$pgid" > "$CARGO_PGID_FILE"
  printf '%s cargo-tree leader=%s grandchild=%s pgid=%s\n' "${COMPOSE_PROJECT_NAME:-missing}" "$$" "$grandchild" "$pgid" >> "$LIFECYCLE_LOG"
  wait "$grandchild"
fi
sleep "${FAKE_CARGO_SLEEP:-0}"
if [ "${FAIL_CARGO:-0}" = 1 ]; then exit 25; fi
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
        .env("CARGO_PGID_FILE", log.with_extension("pgid"))
        .env("DIFFERENTIAL_TERM_GRACE_ATTEMPTS", "2")
        .env("DIFFERENTIAL_TERM_GRACE_INTERVAL", "0.02")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn assert_owned_cleanup(log: &str) {
    let project = log
        .lines()
        .find_map(|line| {
            line.split_once(' ')
                .map(|(project, _)| project)
                .filter(|project| project.starts_with("chp-diff-"))
        })
        .expect("lifecycle project");
    let down = log.find(" down ").expect("cleanup down was attempted");
    let container_scan = log
        .find(&format!(
            "docker ps -aq --filter label=io.openrusty.chp-differential.run={project}"
        ))
        .expect("run-owned oracle containers were discovered");
    let container_remove = log
        .find(&format!("docker rm -f {project}-oracle-container"))
        .expect("unique-image oracle container was removed");
    let image = log
        .find(&format!("docker image rm pingora-chp-oracle:{project}"))
        .expect("unique oracle image was removed");
    assert!(
        container_scan > down,
        "container scan must follow compose down: {log}"
    );
    assert!(
        container_remove > container_scan,
        "container removal must follow scan: {log}"
    );
    assert!(
        image > container_remove,
        "image must follow container removal: {log}"
    );
}

#[test]
fn differential_script_uses_only_run_labels_for_dynamic_oracle_cleanup() {
    let script = std::fs::read_to_string("scripts/test-differential.sh")
        .expect("read standalone differential script");
    assert!(script
        .contains("export CHP_ORACLE_RUN_LABEL=\"io.openrusty.chp-differential.run=$project\""));
    assert!(script.contains("--filter \"label=$CHP_ORACLE_RUN_LABEL\""));
    assert!(!script.contains("--filter \"ancestor="));

    let oracle = std::fs::read_to_string("tests/support/oracle.rs")
        .expect("read differential oracle support");
    assert!(oracle.contains("CHP_ORACLE_RUN_LABEL must be a unique run ownership label"));
    assert!(oracle.contains("command.args([\"--label\", &owner_label])"));

    let differential = std::fs::read_to_string("tests/differential.rs")
        .expect("read differential integration tests");
    let probe_start = differential
        .find("fn oracle_image_runs_actual_node_20_and_exact_chp_source()")
        .expect("runtime probe test");
    let probe_end = differential[probe_start..]
        .find("\nstruct CustomErrorFixture")
        .map(|offset| probe_start + offset)
        .expect("runtime probe test boundary");
    let probe = &differential[probe_start..probe_end];
    assert!(probe.contains("CHP_ORACLE_RUN_LABEL"));
    assert!(probe.contains("oracle_run_owner_label_is_valid_for_test"));
    assert!(probe.contains("\"--label\","));
}

#[test]
fn differential_script_cleans_its_image_at_every_failure_stage_and_success() {
    let tools = fake_tool_directory();
    for (index, failure, expected) in [
        (0, Some("FAIL_BUILD"), 21),
        (1, Some("FAIL_COMPOSE_UP"), 23),
        (2, Some("FAIL_RUNTIME_PROBE"), 24),
        (3, Some("FAIL_CARGO"), 25),
        (4, None, 0),
    ] {
        let log = tools.path().join(format!("lifecycle-{index}.log"));
        let mut command = script_command(&tools, &log);
        if let Some(failure) = failure {
            command.env(failure, "1");
        }
        let status = command.status().expect("run differential lifecycle stage");
        assert_eq!(status.code(), Some(expected), "stage {failure:?}");
        let log = std::fs::read_to_string(log).expect("read lifecycle log");
        assert_owned_cleanup(&log);
    }
}

#[cfg(unix)]
#[test]
fn differential_script_cleans_its_image_on_signal_interruption() {
    let tools = fake_tool_directory();
    let log = tools.path().join("signal.log");
    let mut child = script_command(&tools, &log)
        .env("FAKE_CARGO_TREE", "1")
        .spawn()
        .expect("spawn differential script for signal");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains(" cargo-tree ")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cargo stage not reached"
        );
        std::thread::yield_now();
    }
    let killed = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(killed, 0);
    assert_eq!(child.wait().expect("signal script exit").code(), Some(143));
    std::thread::sleep(std::time::Duration::from_millis(600));
    let log = std::fs::read_to_string(log).expect("read signal lifecycle log");
    assert_owned_cleanup(&log);
    assert!(
        !log.contains("late-container-created"),
        "cargo descendant survived cleanup and created a late container: {log}"
    );
    assert!(
        !log.contains("group-live-at-container-scan"),
        "container scan ran before the cargo process group terminated: {log}"
    );
}

#[cfg(unix)]
#[test]
fn differential_script_reaps_successful_leaders_surviving_process_groups_before_cleanup() {
    let tools = fake_tool_directory();
    let log = tools.path().join("leader-success.log");
    let status = script_command(&tools, &log)
        .env("FAKE_CARGO_LEADER_EXIT_TREE", "1")
        .status()
        .expect("run differential script with successful cargo leader");
    assert!(status.success());
    std::thread::sleep(std::time::Duration::from_millis(600));

    let log = std::fs::read_to_string(log).expect("read leader-success lifecycle log");
    assert!(
        log.contains(" cargo-leader-exit "),
        "leader did not exit early: {log}"
    );
    assert_owned_cleanup(&log);
    assert!(
        !log.contains("late-container-created"),
        "successful cargo descendant survived cleanup: {log}"
    );
    assert!(
        !log.contains("group-live-at-container-scan"),
        "cleanup scanned before the successful cargo group exited: {log}"
    );
}

#[cfg(unix)]
#[test]
fn differential_script_cleans_up_when_early_stages_are_interrupted() {
    let tools = fake_tool_directory();
    for (index, stage, marker) in [
        (0, "build", " compose -f compose.test.yml build chp"),
        (1, "up", " compose -f compose.test.yml up -d redis sidecar"),
        (2, "runtime-probe", " docker run --rm"),
    ] {
        let log = tools.path().join(format!("signal-{index}.log"));
        let mut child = script_command(&tools, &log)
            .env("INTERRUPT_STAGE", stage)
            .env("FAKE_STAGE_DELAY", "0.2")
            .spawn()
            .expect("spawn differential script stage");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains(marker)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "{stage} stage not reached"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        assert_eq!(child.wait().expect("signal script exit").code(), Some(143));
        assert_owned_cleanup(&std::fs::read_to_string(log).expect("stage lifecycle log"));
    }
}

#[test]
fn differential_script_reports_image_removal_failure() {
    let tools = fake_tool_directory();
    for (index, cargo_failure, expected) in [(0, false, 1), (1, true, 25)] {
        let log = tools.path().join(format!("image-rm-failure-{index}.log"));
        let output = script_command(&tools, &log)
            .env("FAIL_IMAGE_RM", "1")
            .env("FAIL_CARGO", if cargo_failure { "1" } else { "0" })
            .stderr(Stdio::piped())
            .output()
            .expect("run image removal failure");
        assert_eq!(output.status.code(), Some(expected));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("failed to remove oracle image"),
            "image removal failure was silent: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
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
        assert!(
            log.lines().any(|line| {
                line.starts_with(&project)
                    && line.contains(&format!(
                        "docker ps -aq --filter label=io.openrusty.chp-differential.run={project}"
                    ))
            }),
            "project {project} did not filter its shared-image containers by owner: {log}"
        );
        assert!(
            log.lines().any(|line| {
                line.starts_with(&project)
                    && line.contains(&format!("docker image rm pingora-chp-oracle:{project}"))
            }),
            "project {project} did not remove only its unique image: {log}"
        );
    }
    assert!(
        !log.contains("ancestor-filter-used"),
        "shared image identity triggered unsafe cross-run cleanup: {log}"
    );
}

#[test]
fn just_recipe_executes_the_standalone_differential_script() {
    let justfile = std::fs::read_to_string("justfile").expect("read justfile");
    assert!(justfile.contains("./scripts/test-differential.sh"));
}

#[test]
fn oracle_image_uses_upstream_lock_and_validates_packed_source_integrity() {
    let compose = std::fs::read_to_string("compose.test.yml").expect("read compose fixture");
    assert!(compose.contains("npm ci --omit=dev"));
    assert!(compose.contains("e55dd25c47058ab05cdcba58b59f4009f49b2fe16f329aa528bf2c233e337cb2"));
    assert!(compose.contains("e5abb83b5d9d10514758d9bd63b1319b4e9361cd429cc3afbe9b4f4d4f37570ecdcfd8f48d4a90430283475c53e7682a5fe5fa0fc52e3bbe7875075491e058b9"));
}
