#![allow(dead_code)]
//! Optional SSHFS mount support for remote SSH workspaces.
//!
//! When `sshfs` + macFUSE are available, mounts the remote workspace
//! directory locally so claude can run on the local machine with full
//! filesystem context. Falls back to the standard remote-PTY approach
//! when sshfs is not installed.
//!
//! Install:
//!   1. Download macFUSE from https://osxfuse.github.io/
//!   2. brew install sshfs
//!
//! Mount path: ~/.ap-mounts/ap-{workspace-id-short}/
//! Uses the existing ControlMaster socket to avoid a second auth.

use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Returns the local mount point for a workspace, whether or not it
/// is currently mounted.
pub fn mount_path_for_workspace(workspace_id: Uuid) -> PathBuf {
    let short = &workspace_id.simple().to_string()[..16];
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".ap-mounts").join(format!("ap-{short}"))
}

/// Returns `true` when sshfs is installed on this machine.
pub fn sshfs_available() -> bool {
    crate::ide_handoff::which_in_path("sshfs").is_some()
}

/// Returns `true` when the mount point has an active FUSE mount.
pub fn is_mounted(mount: &Path) -> bool {
    // A mounted directory has a different device ID from its parent.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = mount.parent().unwrap_or(Path::new("/"));
        let m_dev = std::fs::metadata(mount).map(|m| m.dev()).ok();
        let p_dev = std::fs::metadata(parent).map(|m| m.dev()).ok();
        matches!((m_dev, p_dev), (Some(a), Some(b)) if a != b)
    }
    #[cfg(not(unix))]
    false
}

/// Mount the remote path locally via sshfs, reusing the existing
/// ControlMaster socket for zero-extra-auth access.
///
/// Returns `Ok(mount)` on success. If the directory is already
/// mounted, returns `Ok` immediately (idempotent).
pub fn mount_remote(
    user: Option<&str>,
    host: &str,
    port: Option<u16>,
    remote_path: &str,
    control_socket: &Path,
    workspace_id: Uuid,
) -> Result<PathBuf, String> {
    let mount = mount_path_for_workspace(workspace_id);
    std::fs::create_dir_all(&mount)
        .map_err(|e| format!("create mount dir: {e}"))?;

    if is_mounted(&mount) {
        return Ok(mount);
    }

    let sshfs = crate::ide_handoff::which_in_path("sshfs")
        .ok_or_else(|| "sshfs not found — install macFUSE + sshfs".to_string())?;

    let authority = match user {
        Some(u) => format!("{u}@{host}"),
        None    => host.to_string(),
    };
    let source = format!("{authority}:{remote_path}");
    let socket_str = control_socket.display().to_string();

    let mut cmd = std::process::Command::new(&sshfs);
    cmd.arg(&source)
       .arg(&mount)
       .arg("-o").arg(format!("ControlPath={socket_str}"))
       .arg("-o").arg("reconnect")
       .arg("-o").arg("ServerAliveInterval=5")
       .arg("-o").arg("idmap=user")
       .arg("-o").arg("follow_symlinks");

    if let Some(p) = port {
        cmd.arg("-p").arg(p.to_string());
    }

    let status = cmd.status().map_err(|e| format!("sshfs spawn: {e}"))?;
    if !status.success() {
        return Err(format!("sshfs exited {:?}", status.code()));
    }

    // Give FUSE a moment to complete the mount handshake.
    std::thread::sleep(Duration::from_millis(300));
    if !is_mounted(&mount) {
        return Err("sshfs mount did not become active".to_string());
    }
    Ok(mount)
}

/// Unmount a previously mounted workspace directory.
/// Best-effort: logs and ignores errors (app may already be gone).
pub fn unmount(workspace_id: Uuid) {
    let mount = mount_path_for_workspace(workspace_id);
    if !is_mounted(&mount) {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("diskutil")
            .args(["unmount", "force", &mount.display().to_string()])
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = std::process::Command::new("fusermount")
            .args(["-u", &mount.display().to_string()])
            .status();
    }
}
