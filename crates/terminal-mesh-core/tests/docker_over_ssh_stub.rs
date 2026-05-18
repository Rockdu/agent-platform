//! End-to-end coverage for `DockerOverSshTransport` using a
//! stub-ssh shell script that records every argv-set it receives.
//! The recorder file is the test's window into "what would real
//! OpenSSH have sent over the wire to the remote host", so the
//! tests can assert:
//!
//! - the spawn-time invocation carries the `docker exec -it
//!   <container> /bin/sh -lc '<wrapper>'` shape
//! - the cleanup-time invocation (post-`shutdown(Kill)`) carries
//!   `docker exec <container> sh -lc 'kill ...'`
//! - NO forbidden docker subcommand (stop / run / start / rm /
//!   create) ever appears across either invocation

#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use terminal_mesh_core::{
    transport::{
        ContainerLocation, ShellCommand, ShutdownMode, Transport, TransportError,
        TransportExitStatus, TransportSpawnRequest, WorkspaceLocation,
    },
    DockerOverSshTransport,
};

/// Write a stub-ssh script that appends its full argv (joined by
/// newlines, terminated by a `---` separator) to `recorder` and
/// then runs `script_body` so the test can simulate per-row spec
/// scenarios. The stub's `$@` is what real ssh would have received;
/// the LAST argv slot is the joined remote command string.
fn write_recorder_stub(dir: &Path, recorder: &Path, script_body: &str) -> PathBuf {
    let path = dir.join("stub-ssh");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "RECORDER={}", recorder.display()).unwrap();
    writeln!(f, "for a in \"$@\"; do printf '%s\\n' \"$a\" >> \"$RECORDER\"; done").unwrap();
    writeln!(f, "printf -- '---\\n' >> \"$RECORDER\"").unwrap();
    f.write_all(script_body.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn docker_request(canonical_remote_path: &str, container_id: &str) -> TransportSpawnRequest {
    TransportSpawnRequest {
        workspace: WorkspaceLocation::Remote {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(2222),
            canonical_remote_path: canonical_remote_path.to_string(),
            container: Some(ContainerLocation {
                container_id: container_id.to_string(),
                cwd_in_container: None,
            }),
        },
        command: ShellCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![],
        },
        initial_size: terminal_mesh_core::transport::PtySize { cols: 80, rows: 24 },
        env: BTreeMap::new(),
        cwd: None,
    }
}

fn docker_transport(tmp: &TempDir, recorder: &Path, script_body: &str) -> DockerOverSshTransport {
    let ssh = write_recorder_stub(tmp.path(), recorder, script_body);
    let control_dir = tmp.path().join("ssh-cm");
    std::fs::create_dir_all(&control_dir).unwrap();
    let mut perms = std::fs::metadata(&control_dir).unwrap().permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&control_dir, perms).unwrap();
    DockerOverSshTransport::new(ssh, control_dir)
}

fn read_recorder(recorder: &Path) -> String {
    std::fs::read_to_string(recorder).unwrap_or_default()
}

#[test]
fn docker_spawn_records_docker_exec_invocation() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    // Happy path: emit the SSH sentinel + clean exit so spawn() succeeds.
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let mut session = t
        .spawn(docker_request("/srv", "my-container"))
        .expect("spawn ok");
    let status = session.wait().expect("wait");
    assert!(
        matches!(status, TransportExitStatus::CleanCompletion),
        "expected CleanCompletion, got {status:?}"
    );
    session.cleanup().expect("cleanup");

    let argv = read_recorder(&recorder);
    // The SSH layer's compose_remote_command wraps the docker
    // command in `sh -lc '<escaped-docker-cmd>'`. The docker
    // command's own quotes are POSIX-escaped via `'\''` inside the
    // outer quoting, so the literal substring `'my-container'`
    // becomes `'\''my-container'\''`. Assert on the structural
    // pieces (docker exec + container id + the wrapper sentinel)
    // rather than a single contiguous string.
    assert!(
        argv.contains("docker exec -it"),
        "spawn argv must carry the docker-exec verb; got:\n{argv}"
    );
    assert!(
        argv.contains("my-container"),
        "spawn argv must reference the target container id; got:\n{argv}"
    );
    assert!(
        argv.contains("/bin/sh -lc"),
        "spawn argv must address /bin/sh inside the container; got:\n{argv}"
    );
    assert!(
        argv.contains("am-shell-started"),
        "wrapper script must reach the recorder via the last argv slot"
    );
}

#[test]
fn docker_spawn_argv_never_contains_forbidden_docker_subcommands() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let mut session = t
        .spawn(docker_request("/srv", "my-container"))
        .expect("spawn ok");
    let _ = session.wait();
    session.cleanup().expect("cleanup");

    let argv = read_recorder(&recorder);
    for forbidden in [
        "docker stop",
        "docker run",
        "docker start",
        "docker rm",
        "docker create",
    ] {
        assert!(
            !argv.contains(forbidden),
            "recorded argv must not contain `{forbidden}`; got:\n{argv}",
        );
    }
}

#[test]
fn docker_shutdown_kill_invokes_container_cleanup_via_separate_ssh() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    // Long-running stub so shutdown(Kill) is meaningful. The stub
    // serves BOTH the spawn invocation AND the cleanup invocation
    // (since the cleanup uses the same `ssh_program` we wrote).
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf '\\033]1338;am-shell-started\\a'\nsleep 10\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let mut session = t
        .spawn(docker_request("/srv", "my-container"))
        .expect("spawn ok");
    std::thread::sleep(std::time::Duration::from_millis(100));
    session.shutdown(ShutdownMode::Kill).expect("shutdown");
    let _ = session.wait();
    session.cleanup().expect("cleanup");

    // Cleanup ssh runs on a detached background thread with a
    // 5-second deadline (the non-blocking cleanup design). Poll
    // the recorder until both spawn and cleanup argv appear, or
    // until the deadline. The previous fixed sleep in `wait()`
    // was implicitly serving as the wait-for-cleanup window;
    // once `wait()` became a synchronous reader-join (so
    // fast-exit sessions classify correctly), the
    // cleanup-async-vs-test race needs an explicit polling loop
    // here in the test instead.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut argv = String::new();
    while std::time::Instant::now() < deadline {
        argv = read_recorder(&recorder);
        if argv.contains("docker exec") && argv.contains("kill") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // Spawn invocation: docker exec -it ... + the SSH wrapper.
    assert!(
        argv.contains("docker exec -it") && argv.contains("my-container") && argv.contains("/bin/sh -lc"),
        "spawn argv missing docker-exec-it/container/sh; got:\n{argv}",
    );
    // Cleanup invocation: docker exec <container> sh -lc 'kill ...'.
    // The `-it` flag is OMITTED on cleanup (no TTY needed for a
    // one-shot kill); the test asserts the cleanup-shape pieces.
    assert!(
        argv.contains("docker exec") && argv.contains("my-container") && argv.contains("sh -lc")
            && argv.contains("kill"),
        "cleanup argv missing docker exec/container/sh/kill; got:\n{argv}",
    );
    assert!(
        argv.contains("/tmp/agentmesh-wrapper-") && argv.contains(".pid"),
        "cleanup must reference the per-session PID file; got:\n{argv}",
    );
    // And still no forbidden docker subcommand.
    for forbidden in [
        "docker stop",
        "docker run",
        "docker start",
        "docker rm",
        "docker create",
    ] {
        assert!(
            !argv.contains(forbidden),
            "lifecycle must not emit `{forbidden}`; got:\n{argv}",
        );
    }
}

#[test]
fn docker_local_workspace_rejected_with_protocol_error() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(&tmp, &recorder, "exit 0\n");
    let local_req = TransportSpawnRequest {
        workspace: WorkspaceLocation::Local { path: None },
        command: ShellCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![],
        },
        initial_size: terminal_mesh_core::transport::PtySize { cols: 80, rows: 24 },
        env: BTreeMap::new(),
        cwd: None,
    };
    let err = match t.spawn(local_req) {
        Ok(_) => panic!("must reject Local"),
        Err(e) => e,
    };
    assert!(
        matches!(err, TransportError::Protocol { .. }),
        "expected Protocol; got {err:?}"
    );
}

#[test]
fn docker_missing_container_returns_typed_docker_error() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf 'Error response from daemon: No such container: my-container\\n' >&2\nexit 1\n",
    );
    let err = match t.spawn(docker_request("/srv", "my-container")) {
        Ok(_) => panic!("must reject missing container"),
        Err(e) => e,
    };
    match err {
        TransportError::DockerContainerMissing { container } => {
            assert_eq!(container, "my-container");
        }
        other => panic!("expected DockerContainerMissing; got {other:?}"),
    }
}

#[test]
fn docker_missing_binary_returns_typed_docker_error() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf 'sh: docker: command not found\\n' >&2\nexit 127\n",
    );
    let err = match t.spawn(docker_request("/srv", "my-container")) {
        Ok(_) => panic!("must reject missing docker"),
        Err(e) => e,
    };
    assert!(
        matches!(err, TransportError::DockerExecFailed { .. }),
        "expected DockerExecFailed; got {err:?}"
    );
}

/// Lifecycle-modeling regression per spec §5.3: prove that
/// `shutdown(Kill)` triggers the cleanup invocation which actually
/// terminates a separately-modeled "remote process" within 5s of
/// `t0` (measured BEFORE shutdown).
///
/// The stub-ssh script dispatches on its remote-command argv:
///
/// - The CLEANUP invocation (last argv contains `kill` AND
///   `agentmesh-wrapper`) reads `REMOTE_PID_FILE` and sends
///   `kill -9` to the modeled remote process. This mirrors what
///   `docker exec <container> kill <wrapper-pid>` would do against
///   a real container.
///
/// - The SPAWN invocation forks a child that writes its own PID
///   to `REMOTE_PID_FILE` and sleeps under `trap '' HUP TERM` (so
///   the cleanup-kill is the ONLY thing that can terminate it).
///   The stub-ssh process itself stays alive while its modeled
///   remote child runs.
///
/// The test:
///   1. Spawn the session; wait for the modeled remote PID file.
///   2. Record `t0 = Instant::now()` BEFORE `shutdown(Kill)`.
///   3. Call `shutdown(Kill)` — must return well before `t0+5s`
///      because the cleanup dispatcher is non-blocking.
///   4. Poll `kill -0 <modeled-remote-pid>` against `t0+5s`.
///   5. Assert ESRCH within the window (i.e. the cleanup invocation
///      actually killed the modeled remote process).
#[test]
fn docker_shutdown_kill_kills_modeled_remote_process_within_5s() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let remote_pid_file = tmp.path().join("remote.pid");
    // The stub script:
    // - Always records argv into the recorder (set up by write_recorder_stub-like prelude).
    // - Dispatches on the last argv element.
    let stub_body = format!(
        r#"REMOTE_PID_FILE='{rpf}'
last_arg=""
for a in "$@"; do last_arg="$a"; done
case "$last_arg" in
  *"kill"*"agentmesh-wrapper"*)
    if [ -f "$REMOTE_PID_FILE" ]; then
      kill -9 "$(cat "$REMOTE_PID_FILE")" 2>/dev/null || true
      rm -f "$REMOTE_PID_FILE"
    fi
    exit 0
    ;;
  *)
    (trap '' HUP TERM; sleep 30) &
    REMOTE_PID=$!
    echo "$REMOTE_PID" > "$REMOTE_PID_FILE"
    printf '\033]1338;am-shell-started\a'
    wait "$REMOTE_PID" 2>/dev/null
    printf '\033]1338;am-exit-status;0\a'
    exit 0
    ;;
esac
"#,
        rpf = remote_pid_file.display(),
    );
    let t = docker_transport(&tmp, &recorder, &stub_body);
    let mut session = t
        .spawn(docker_request("/srv", "my-container"))
        .expect("spawn ok");

    // Wait for the modeled remote to publish its PID.
    let remote_pid: i32 = {
        let mut pid: Option<i32> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&remote_pid_file)
                && let Ok(n) = s.trim().parse::<i32>()
            {
                pid = Some(n);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        pid.expect("modeled remote process must record its PID within 2s")
    };

    // T0 BEFORE shutdown so the 5s budget covers cleanup dispatch
    // + the actual kill in one window.
    let t0 = std::time::Instant::now();
    session.shutdown(ShutdownMode::Kill).expect("shutdown");
    // The shutdown contract: must return promptly. If it blocked on
    // cleanup, this single assertion would fail before the polling
    // loop even runs.
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "shutdown(Kill) must NOT block on cleanup; took {:?}",
        t0.elapsed(),
    );

    let _ = session.wait();
    session.cleanup().expect("cleanup");

    // Poll for the modeled remote process to be reaped within the
    // 5s window measured from BEFORE shutdown.
    let deadline = t0 + std::time::Duration::from_secs(5);
    let mut still_alive = true;
    while std::time::Instant::now() < deadline {
        let probe = std::process::Command::new("kill")
            .arg("-0")
            .arg(remote_pid.to_string())
            .output()
            .expect("kill -0 probe");
        if !probe.status.success() {
            still_alive = false;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !still_alive,
        "modeled remote process pid={remote_pid} must be terminated by the cleanup invocation within 5s of shutdown(Kill)",
    );

    // The cleanup ssh invocation reached the recorder.
    let argv = read_recorder(&recorder);
    assert!(
        argv.contains("docker exec") && argv.contains("kill") && argv.contains("rm -f"),
        "cleanup ssh invocation must reach the recorder; got:\n{argv}",
    );
    for forbidden in [
        "docker stop",
        "docker run",
        "docker start",
        "docker rm",
        "docker create",
    ] {
        assert!(
            !argv.contains(forbidden),
            "lifecycle must not emit `{forbidden}`; got:\n{argv}",
        );
    }
}

#[test]
fn docker_remote_without_container_rejected_with_protocol_error() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(&tmp, &recorder, "exit 0\n");
    let remote_no_ctr = TransportSpawnRequest {
        workspace: WorkspaceLocation::Remote {
            user: None,
            host: "h.example".into(),
            port: None,
            canonical_remote_path: "/srv".into(),
            container: None,
        },
        command: ShellCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![],
        },
        initial_size: terminal_mesh_core::transport::PtySize { cols: 80, rows: 24 },
        env: BTreeMap::new(),
        cwd: None,
    };
    let err = match t.spawn(remote_no_ctr) {
        Ok(_) => panic!("must reject Remote-without-container"),
        Err(e) => e,
    };
    assert!(
        matches!(err, TransportError::Protocol { .. }),
        "expected Protocol; got {err:?}"
    );
}

/// When the spawn request carries a non-empty `ShellCommand`, the
/// docker wrapper must invoke it inside the container (after the
/// shell-started sentinel) instead of falling through to a login
/// shell. Verifies the same exec-replacement that
/// `ssh_transport_runs_requested_command_when_provided` covers for
/// the SSH path, but inside the `docker exec -it <container> ...`
/// envelope.
#[test]
fn docker_transport_runs_requested_command_when_provided() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let req = TransportSpawnRequest {
        workspace: WorkspaceLocation::Remote {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(2222),
            canonical_remote_path: "/srv".into(),
            container: Some(ContainerLocation {
                container_id: "my-container".into(),
                cwd_in_container: None,
            }),
        },
        command: ShellCommand {
            program: PathBuf::from("/usr/bin/claude"),
            args: vec!["--dangerously-skip-permissions".into()],
        },
        initial_size: terminal_mesh_core::transport::PtySize { cols: 80, rows: 24 },
        env: BTreeMap::new(),
        cwd: None,
    };
    let mut session = t.spawn(req).expect("spawn ok");
    let status = session.wait().expect("wait");
    assert!(
        matches!(status, TransportExitStatus::CleanCompletion),
        "expected CleanCompletion, got {status:?}"
    );
    session.cleanup().expect("cleanup");

    let argv = read_recorder(&recorder);
    assert!(
        argv.contains("/usr/bin/claude"),
        "spawn argv must invoke the requested program inside the container; got:\n{argv}"
    );
    assert!(
        argv.contains("--dangerously-skip-permissions"),
        "spawn argv must carry the requested argv element; got:\n{argv}"
    );
    assert!(
        argv.contains("docker exec -it"),
        "spawn argv must still wrap the command in docker exec; got:\n{argv}"
    );
    assert!(
        argv.contains("my-container"),
        "spawn argv must target the configured container; got:\n{argv}"
    );
    assert!(
        !argv.contains("$SHELL"),
        "exec mode must not fall through to the login-shell branch; got:\n{argv}"
    );
}

/// Interactive Docker shell tabs reach the dispatcher with an
/// empty `ShellCommand`. The wrapper must take the login-shell
/// branch inside the `docker exec -it <container> /bin/sh -lc
/// '...'` envelope. Mirror of
/// `ssh_transport_runs_login_shell_when_command_is_empty` from
/// the SSH stub suite, but pinned at the Docker envelope level.
#[test]
fn docker_transport_runs_login_shell_when_command_is_empty() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let req = TransportSpawnRequest {
        workspace: WorkspaceLocation::Remote {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(2222),
            canonical_remote_path: "/srv".into(),
            container: Some(ContainerLocation {
                container_id: "my-container".into(),
                cwd_in_container: None,
            }),
        },
        command: ShellCommand {
            program: std::path::PathBuf::new(),
            args: vec![],
        },
        initial_size: terminal_mesh_core::transport::PtySize { cols: 80, rows: 24 },
        env: BTreeMap::new(),
        cwd: None,
    };
    let mut session = t.spawn(req).expect("spawn ok");
    let status = session.wait().expect("wait");
    assert!(
        matches!(status, TransportExitStatus::CleanCompletion),
        "expected CleanCompletion, got {status:?}"
    );
    session.cleanup().expect("cleanup");

    let argv = read_recorder(&recorder);
    assert!(
        argv.contains("\"$SHELL\" -l"),
        "docker wrapper must take the login-shell branch when command is empty; got:\n{argv}"
    );
    assert!(
        argv.contains("docker exec -it"),
        "wrapper must still be inside the docker exec envelope; got:\n{argv}"
    );
    assert!(
        argv.contains("my-container"),
        "wrapper must target the configured container; got:\n{argv}"
    );
    assert!(
        !argv.contains("/bin/zsh") && !argv.contains("/bin/bash"),
        "wrapper must NOT contain a caller-supplied local shell path; got:\n{argv}"
    );
}

/// `probe` on a reachable Docker workspace must return `Ok(())`.
/// The recorder stub mimics a clean lifecycle: shell-started +
/// exit-status sentinels + exit 0.
#[test]
fn docker_transport_probe_succeeds_for_reachable_workspace() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let workspace = WorkspaceLocation::Remote {
        user: Some("alice".into()),
        host: "h.example".into(),
        port: Some(2222),
        canonical_remote_path: "/srv".into(),
        container: Some(ContainerLocation {
            container_id: "my-container".into(),
            cwd_in_container: None,
        }),
    };
    t.probe(workspace).expect("probe must accept a reachable container");
}

/// `probe` MUST surface `DockerContainerMissing` when the
/// docker exec stderr matches the `No such container` pattern
/// the Docker-aware Phase-A classifier recognizes.
#[test]
fn docker_transport_probe_returns_typed_container_missing_error() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf 'Error response from daemon: No such container: missing-id\\n' >&2\nexit 1\n",
    );
    let workspace = WorkspaceLocation::Remote {
        user: Some("alice".into()),
        host: "h.example".into(),
        port: Some(2222),
        canonical_remote_path: "/srv".into(),
        container: Some(ContainerLocation {
            container_id: "missing-id".into(),
            cwd_in_container: None,
        }),
    };
    match t.probe(workspace) {
        Ok(()) => panic!("probe must reject missing container"),
        Err(TransportError::DockerContainerMissing { container }) => {
            assert_eq!(container, "missing-id");
        }
        Err(other) => panic!("expected DockerContainerMissing; got {other:?}"),
    }
}

/// `probe` MUST surface `DockerExecFailed` when the remote
/// stderr matches `docker: command not found` (one of the
/// patterns the Docker-aware classifier recognizes).
#[test]
fn docker_transport_probe_returns_typed_exec_failed_when_docker_missing() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let t = docker_transport(
        &tmp,
        &recorder,
        "printf 'sh: 1: docker: command not found\\n' >&2\nexit 127\n",
    );
    let workspace = WorkspaceLocation::Remote {
        user: Some("alice".into()),
        host: "h.example".into(),
        port: Some(2222),
        canonical_remote_path: "/srv".into(),
        container: Some(ContainerLocation {
            container_id: "my-container".into(),
            cwd_in_container: None,
        }),
    };
    match t.probe(workspace) {
        Ok(()) => panic!("probe must reject missing docker binary"),
        Err(TransportError::DockerExecFailed { .. }) => {}
        Err(other) => panic!("expected DockerExecFailed; got {other:?}"),
    }
}
