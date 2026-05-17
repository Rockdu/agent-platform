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
        ShellCommand, ShutdownMode, Transport, TransportError, TransportExitStatus,
        TransportSpawnRequest, WorkspaceLocation,
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
            container: Some(container_id.to_string()),
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
