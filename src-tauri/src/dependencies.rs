//! Dependency checker and one-click installer for Agent Platform.
//!
//! Required dependencies:
//!   - Homebrew      (package manager; all others installed via brew)
//!   - tmux          (session persistence: brew install tmux)
//!   - Claude Code   (core: brew install claude / manual install)
//!   - macFUSE       (remote SSHFS: brew install --cask macfuse)
//!   - sshfs         (remote SSHFS: brew install sshfs)
//!
//! macFUSE requires a kernel-extension approval in System Settings →
//! Privacy & Security after installation; the UI shows a reminder.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use serde::{Deserialize, Serialize};
use tauri::State;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DepKind {
    Homebrew,
    Tmux,
    Claude,
    MacFuse,
    Sshfs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepInfo {
    pub kind: DepKind,
    pub label: String,
    pub description: String,
    pub installed: bool,
    pub version: Option<String>,
    /// Whether the app core function fails without this dependency.
    pub required: bool,
    /// Extra note shown after installation (e.g. kernel-ext warning).
    pub post_install_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub kind: DepKind,
    pub success: bool,
    pub output: String,
    pub post_install_note: Option<String>,
}

// ---------------------------------------------------------------------------
// Detection helpers
// ---------------------------------------------------------------------------

fn which_brew() -> Option<PathBuf> {
    for p in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] {
        let path = Path::new(p);
        if path.exists() {
            return Some(path.to_path_buf());
        }
    }
    crate::ide_handoff::which_in_path("brew")
}

fn brew_list_version(package: &str) -> Option<String> {
    let brew = which_brew()?;
    let out = Command::new(&brew)
        .args(["list", "--versions", package])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn brew_cask_installed(cask: &str) -> bool {
    let Some(brew) = which_brew() else { return false };
    let out = Command::new(&brew)
        .args(["list", "--cask", cask])
        .output()
        .ok();
    out.map(|o| o.status.success()).unwrap_or(false)
}

fn macfuse_installed() -> bool {
    // macFUSE installs a system framework.
    Path::new("/Library/Filesystems/macFUSE.fs").exists()
        || brew_cask_installed("macfuse")
}

fn simple_version(cmd: &str, arg: &str) -> Option<String> {
    let out = Command::new(cmd).arg(arg).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if s.is_empty() { None } else { Some(s) }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn check_all() -> Vec<DepInfo> {
    let brew = which_brew();
    let brew_ok = brew.is_some();
    let brew_ver = brew.as_ref().and_then(|b| {
        Command::new(b).arg("--version").output().ok().and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_string())
        })
    });

    let tmux_ok = crate::ide_handoff::which_in_path("tmux").is_some();
    let tmux_ver = simple_version("tmux", "-V");

    let claude_ok = crate::ide_handoff::which_in_path("claude").is_some()
        || Path::new("/usr/local/bin/claude").exists()
        || Path::new("/opt/homebrew/bin/claude").exists();
    let claude_ver = simple_version("claude", "--version");

    let macfuse_ok = macfuse_installed();
    // Check both the standard sshfs and the macOS gromgit variant.
    let sshfs_ok = crate::ide_handoff::which_in_path("sshfs").is_some()
        || brew_list_version("gromgit/fuse/sshfs-mac").is_some();
    let sshfs_ver = brew_list_version("gromgit/fuse/sshfs-mac")
        .or_else(|| brew_list_version("sshfs"));

    vec![
        DepInfo {
            kind: DepKind::Homebrew,
            label: "Homebrew".into(),
            description: "macOS 包管理器，其他所有依赖通过 brew 安装".into(),
            installed: brew_ok,
            version: brew_ver,
            required: true,
            post_install_note: None,
        },
        DepInfo {
            kind: DepKind::Claude,
            label: "Claude Code (claude CLI)".into(),
            description: "核心：每个工作区运行的 AI 编程助手".into(),
            installed: claude_ok,
            version: claude_ver,
            required: true,
            post_install_note: Some(
                "安装后可能需要重新登录：claude login".into(),
            ),
        },
        DepInfo {
            kind: DepKind::Tmux,
            label: "tmux".into(),
            description: "终端复用器，保证 app 重启后 claude 会话不丢失".into(),
            installed: tmux_ok,
            version: tmux_ver,
            required: true,
            post_install_note: None,
        },
        DepInfo {
            kind: DepKind::MacFuse,
            label: "macFUSE".into(),
            description: "FUSE 内核扩展，支持 SSHFS 挂载远程目录".into(),
            installed: macfuse_ok,
            version: if macfuse_ok { Some("已安装".into()) } else { None },
            required: false,
            post_install_note: Some(
                "安装后需在「系统设置 → 隐私与安全性」中允许内核扩展，然后重启".into(),
            ),
        },
        DepInfo {
            kind: DepKind::Sshfs,
            label: "sshfs".into(),
            description: "SSH 文件系统挂载，让远程工作区在本地运行 claude（需要 macFUSE）".into(),
            installed: sshfs_ok,
            version: sshfs_ver,
            required: false,
            post_install_note: None,
        },
    ]
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_dependency_status() -> Vec<DepInfo> {
    check_all()
}

#[tauri::command]
pub async fn install_dependency(kind: DepKind) -> InstallResult {
    let post_note = check_all()
        .into_iter()
        .find(|d| d.kind == kind)
        .and_then(|d| d.post_install_note.clone());

    // Run the blocking brew command on the thread pool so we don't
    // block the Tokio async executor (brew can take several minutes).
    let kind_clone = kind.clone();
    let (success, output) = tokio::task::spawn_blocking(move || match kind_clone {
        DepKind::Homebrew => install_homebrew(),
        DepKind::Tmux     => brew_install("tmux", false),
        DepKind::Claude   => brew_install("claude", false),
        DepKind::MacFuse  => brew_install("macfuse", true),
        // On macOS, the standard `brew install sshfs` fails (Linux only).
        // Use the gromgit/fuse tap which provides a macOS-native build.
        DepKind::Sshfs    => install_sshfs_macos(),
    })
    .await
    .unwrap_or_else(|e| (false, format!("task panicked: {e}")));

    InstallResult { kind, success, output, post_install_note: post_note }
}

fn brew_install(package: &str, is_cask: bool) -> (bool, String) {
    let Some(brew) = which_brew() else {
        return (false, "Homebrew 未安装，请先安装 Homebrew".into());
    };
    let mut cmd = Command::new(&brew);
    cmd.env("HOMEBREW_NO_ENV_HINTS", "1"); // suppress env hint noise
    if is_cask {
        cmd.args(["install", "--cask", package]);
    } else {
        cmd.args(["install", package]);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}{stderr}").trim().to_string();
            (out.status.success(), combined)
        }
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

/// On macOS, sshfs from Homebrew core requires Linux.
/// Use the gromgit/fuse tap which maintains a macOS-native sshfs build.
fn install_sshfs_macos() -> (bool, String) {
    let Some(brew) = which_brew() else {
        return (false, "Homebrew 未安装，请先安装 Homebrew".into());
    };
    if !macfuse_installed() {
        return (false, "请先安装 macFUSE，然后允许内核扩展并重启，再安装 sshfs".into());
    }
    // 1. Add the tap
    let tap = Command::new(&brew)
        .args(["tap", "gromgit/fuse"])
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let tap_ok = tap.as_ref().map(|o| o.status.success()).unwrap_or(false);
    if !tap_ok {
        let err = tap.map(|o| {
            let s = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let r = String::from_utf8_lossy(&o.stdout).trim().to_string();
            format!("{s}{r}")
        }).unwrap_or_else(|e| e.to_string());
        return (false, format!("brew tap gromgit/fuse 失败: {err}"));
    }
    // 2. Install sshfs-mac from the tap
    let out = Command::new(&brew)
        .args(["install", "gromgit/fuse/sshfs-mac"])
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{stdout}{stderr}").trim().to_string();
            (o.status.success(), combined)
        }
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

fn install_homebrew() -> (bool, String) {
    // Homebrew installation requires user interaction (sudo).
    // Open the official install page and copy the command to clipboard.
    let script = "/bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"";
    // Try to open a new Terminal window with the command pre-filled.
    let result = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                r#"tell application "Terminal" to do script "{script}""#
            ),
        ])
        .output();
    match result {
        Ok(o) if o.status.success() => (
            true,
            "已在 Terminal 中打开安装脚本，请在 Terminal 中完成安装".into(),
        ),
        _ => (
            false,
            format!("请手动在 Terminal 运行：\n{script}"),
        ),
    }
}
