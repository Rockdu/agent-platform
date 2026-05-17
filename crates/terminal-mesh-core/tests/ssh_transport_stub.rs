//! End-to-end coverage for `SshTransport` using a stub-ssh shell
//! script in place of the system `ssh` binary. The stub script
//! receives the same argv `SshTransport::spawn` would pass to real
//! ssh, then emits a scripted stdout/stderr/exit pattern that
//! exercises one of the spec §4.6 rows. The transport's spawn-time
//! pre-shell phase gate and wait-time post-shell phase classifier are exercised
//! directly without needing a real SSH endpoint.

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
    SshTransport,
};

fn write_stub_ssh(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("stub-ssh");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    f.write_all(body.as_bytes()).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn remote_request(canonical_remote_path: &str) -> TransportSpawnRequest {
    TransportSpawnRequest {
        workspace: WorkspaceLocation::Remote {
            user: Some("alice".into()),
            host: "h.example".into(),
            port: Some(2222),
            canonical_remote_path: canonical_remote_path.to_string(),
            container: None,
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

fn transport(tmp: &TempDir, body: &str) -> SshTransport {
    let ssh = write_stub_ssh(tmp.path(), body);
    let control_dir = tmp.path().join("ssh-cm");
    std::fs::create_dir_all(&control_dir).unwrap();
    let mut perms = std::fs::metadata(&control_dir).unwrap().permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(&control_dir, perms).unwrap();
    SshTransport::new(ssh, control_dir)
}

#[test]
fn ssh_transport_happy_path_clean_completion() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let mut session = t.spawn(remote_request("/srv")).expect("spawn ok");
    let status = session.wait().expect("wait");
    assert!(
        matches!(status, TransportExitStatus::CleanCompletion),
        "expected CleanCompletion, got {status:?}"
    );
    session.cleanup().expect("cleanup");
}

#[test]
fn ssh_transport_non_zero_exit_status_classified_as_non_zero() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf '\\033]1338;am-shell-started\\a'\nprintf '\\033]1338;am-exit-status;42\\a'\nexit 42\n",
    );
    let mut session = t.spawn(remote_request("/srv")).expect("spawn ok");
    let status = session.wait().expect("wait");
    match status {
        TransportExitStatus::NonZeroExit(n) => assert_eq!(n, 42),
        other => panic!("expected NonZeroExit(42); got {other:?}"),
    }
    session.cleanup().expect("cleanup");
}

#[test]
fn ssh_transport_auth_failure_returns_typed_err() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf 'alice@h.example: Permission denied (publickey).\\n' >&2\nexit 255\n",
    );
    let err = match t.spawn(remote_request("/srv")) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    match err {
        TransportError::SshAuth { user, host, port } => {
            assert_eq!(user, "alice");
            assert_eq!(host, "h.example");
            assert_eq!(port, 2222);
        }
        other => panic!("expected SshAuth; got {other:?}"),
    }
}

#[test]
fn ssh_transport_host_key_changed_returns_typed_err() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf '@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\\n' >&2\nexit 255\n",
    );
    let err = match t.spawn(remote_request("/srv")) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    match err {
        TransportError::SshHostKeyChanged { host, port, message } => {
            assert_eq!(host, "h.example");
            assert_eq!(port, 2222);
            assert!(message.contains("REMOTE HOST"));
        }
        other => panic!("expected SshHostKeyChanged; got {other:?}"),
    }
}

#[test]
fn ssh_transport_connect_refused_returns_typed_err() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf 'ssh: connect to host h.example port 2222: Connection refused\\n' >&2\nexit 255\n",
    );
    let err = match t.spawn(remote_request("/srv")) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    assert!(
        matches!(err, TransportError::SshConnect { .. }),
        "expected SshConnect; got {err:?}"
    );
}

#[test]
fn ssh_transport_exit_77_maps_to_remote_path_invalid() {
    let tmp = TempDir::new().unwrap();
    let t = transport(&tmp, "exit 77\n");
    let err = match t.spawn(remote_request("/nope")) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    match err {
        TransportError::RemotePathInvalid {
            path,
            host,
            port,
            ..
        } => {
            assert_eq!(path, "/nope");
            assert_eq!(host, "h.example");
            assert_eq!(port, 2222);
        }
        other => panic!("expected RemotePathInvalid; got {other:?}"),
    }
}

#[test]
fn ssh_transport_unknown_failure_classified_as_catchall() {
    let tmp = TempDir::new().unwrap();
    let t = transport(&tmp, "echo something unrelated >&2\nexit 2\n");
    let err = match t.spawn(remote_request("/srv")) {
        Ok(_) => panic!("must reject"),
        Err(e) => e,
    };
    match err {
        TransportError::SshShellDidNotStart {
            host,
            port,
            ssh_exit,
            ..
        } => {
            assert_eq!(host, "h.example");
            assert_eq!(port, 2222);
            assert_eq!(ssh_exit, Some(2));
        }
        other => panic!("expected SshShellDidNotStart; got {other:?}"),
    }
}

/// Pre-shell/post-shell boundary: ssh emits `am-shell-started` then
/// exits without an `am-exit-status` sentinel. `spawn` must succeed
/// (pre-shell phase OK); `wait` must surface a Disconnect through
/// the post-shell phase path.
#[test]
fn ssh_transport_phase_boundary_post_shell_disconnect_returns_ok_then_disconnect() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf '\\033]1338;am-shell-started\\a'\nexit 9\n",
    );
    let mut session = t.spawn(remote_request("/srv")).expect("phase A OK");
    let status = session.wait().expect("wait");
    assert!(
        matches!(status, TransportExitStatus::Disconnect(_)),
        "expected Disconnect after lost channel; got {status:?}"
    );
    session.cleanup().expect("cleanup");
}

#[test]
fn ssh_transport_local_workspace_rejected_with_protocol_error() {
    let tmp = TempDir::new().unwrap();
    let t = transport(&tmp, "exit 0\n");
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
fn ssh_transport_local_transport_rejects_remote_workspace() {
    use terminal_mesh_core::transport::LocalTransport;
    let lt = LocalTransport::new();
    let err = match lt.spawn(remote_request("/srv")) {
        Ok(_) => panic!("Local must reject Remote"),
        Err(e) => e,
    };
    assert!(
        matches!(err, TransportError::Protocol { .. }),
        "expected Protocol; got {err:?}"
    );
}

#[test]
fn ssh_transport_preserves_bursty_output_across_short_reads() {
    let tmp = TempDir::new().unwrap();
    // Stub emits the shell-started sentinel, then a deterministic
    // 32 KiB payload, then the exit-status sentinel. The consumer
    // reads in 4 KiB chunks; every byte must arrive in order.
    let stub_body = r#"printf '\033]1338;am-shell-started\a'
i=0
while [ $i -lt 1024 ]; do
  printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZ012345'
  i=$((i+1))
done
printf '\033]1338;am-exit-status;0\a'
exit 0
"#;
    let t = transport(&tmp, stub_body);
    let mut session = t.spawn(remote_request("/srv")).expect("spawn ok");
    let mut reader = session.output_stream().expect("output_stream");
    let mut accum: Vec<u8> = Vec::with_capacity(32 * 1024);
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => accum.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if std::time::Instant::now() > deadline {
            break;
        }
        if accum.len() >= 32 * 1024 {
            break;
        }
    }
    // Strip the OSC 1338 sentinels and ssh's own banner before
    // comparing. The pattern body is 32 KiB of repeated 32-byte
    // chunks; assert that the count of expected payload bytes is
    // preserved (no drops).
    let payload = "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
    let needle = payload.as_bytes();
    let mut count = 0usize;
    let mut window = &accum[..];
    while let Some(idx) = window
        .windows(needle.len())
        .position(|w| w == needle)
    {
        count += 1;
        window = &window[idx + needle.len()..];
    }
    assert_eq!(
        count, 1024,
        "expected 1024 repetitions of the 32-byte payload; got {count} (out of {} bytes)",
        accum.len(),
    );
    let _ = session.wait();
    session.cleanup().expect("cleanup");
}

#[test]
fn ssh_transport_shutdown_terminates_long_running_session() {
    let tmp = TempDir::new().unwrap();
    let t = transport(
        &tmp,
        "printf '\\033]1338;am-shell-started\\a'\nsleep 30\nprintf '\\033]1338;am-exit-status;0\\a'\nexit 0\n",
    );
    let mut session = t.spawn(remote_request("/srv")).expect("phase A OK");
    std::thread::sleep(std::time::Duration::from_millis(100));
    session.shutdown(ShutdownMode::Kill).expect("shutdown");
    let status = session.wait().expect("wait");
    assert!(
        !matches!(status, TransportExitStatus::CleanCompletion),
        "expected non-clean exit after Kill; got {status:?}"
    );
    session.cleanup().expect("cleanup");
}
