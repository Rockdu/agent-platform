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

    let argv = read_recorder(&recorder);
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

/// Lifecycle-modeling regression per spec §5.3: the stub records
/// its own PID, ignores cooperative termination signals
/// (`trap '' HUP TERM`), then sleeps long enough that the spawn
/// state is unambiguously "running" when the test calls
/// `shutdown(Kill)`. After Kill + a 5s poll deadline, the test
/// verifies the modeled spawn process is GONE (kill -0 returns
/// ESRCH) AND that the cleanup ssh invocation was recorded.
#[test]
fn docker_shutdown_kill_terminates_spawn_within_5s() {
    let tmp = TempDir::new().unwrap();
    let recorder = tmp.path().join("argv.log");
    let pid_file = tmp.path().join("spawn.pid");
    // Stub: write own pid, ignore cooperative signals, emit the
    // shell-started sentinel so spawn() returns Ok, then sleep.
    let body = format!(
        "echo $$ > {pid_file}\ntrap '' HUP TERM\nprintf '\\033]1338;am-shell-started\\a'\nsleep 30\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
        pid_file = pid_file.display(),
    );
    let t = docker_transport(&tmp, &recorder, &body);
    let mut session = t
        .spawn(docker_request("/srv", "my-container"))
        .expect("spawn ok");

    // Wait briefly for the stub to actually record its PID.
    let pid: i32 = {
        let mut pid: Option<i32> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&pid_file)
                && let Ok(n) = s.trim().parse::<i32>()
            {
                pid = Some(n);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        pid.expect("stub spawn must record its PID within 2s")
    };

    // Trigger Kill and poll for termination.
    session.shutdown(ShutdownMode::Kill).expect("shutdown");
    let _ = session.wait();
    session.cleanup().expect("cleanup");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut still_alive = true;
    while std::time::Instant::now() < deadline {
        // `kill -0` returns 0 if the process exists, ESRCH otherwise.
        // We exec /bin/kill instead of the shell builtin so the
        // probe works regardless of the test environment's shell.
        let probe = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
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
        "spawn process pid={pid} must be terminated within 5s of shutdown(Kill)",
    );

    // And the cleanup ssh invocation still reached the recorder.
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
