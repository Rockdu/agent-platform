//! Dependency checker and one-click installer for Agent Platform.
//!
//! Required:
//!   - Homebrew     (package manager)
//!   - tmux         (session persistence)
//!   - Claude Code  (core AI agent)

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DepKind {
    Homebrew,
    Tmux,
    Claude,
    /// Docker CLI (no daemon). Required for DOCKER_HOST=ssh:// tunneling so
    /// the Dev Containers extension can open remote containers in Cursor.
    /// Install: brew install docker  (just the CLI, not Docker Desktop)
    DockerCli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepInfo {
    pub kind: DepKind,
    pub label: String,
    pub description: String,
    pub installed: bool,
    pub version: Option<String>,
    pub required: bool,
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

#[allow(dead_code)]
fn brew_list_version(package: &str) -> Option<String> {
    let brew = which_brew()?;
    let out = Command::new(&brew)
        .args(["list", "--versions", package])
        .env("HOMEBREW_NO_ENV_HINTS", "1")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
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
    let brew_ok = which_brew().is_some();
    let brew_ver = which_brew().and_then(|b| {
        Command::new(&b)
            .arg("--version")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .map(|l| l.trim().to_string())
            })
    });

    let docker_ok = crate::ide_handoff::which_in_path("docker").is_some();
    let docker_ver = simple_version("docker", "--version");

    let tmux_ok = crate::ide_handoff::which_in_path("tmux").is_some();
    let tmux_ver = simple_version("tmux", "-V");

    let claude_ok = crate::ide_handoff::which_in_path("claude").is_some()
        || Path::new("/usr/local/bin/claude").exists()
        || Path::new("/opt/homebrew/bin/claude").exists();
    let claude_ver = simple_version("claude", "--version");

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
            post_install_note: Some("安装后需要运行：claude login".into()),
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
            kind: DepKind::DockerCli,
            label: "Docker CLI".into(),
            description: "Docker 命令行工具（不需要 Docker Desktop）。让 Cursor 通过 SSH 直接 attach 进远端容器所必需".into(),
            installed: docker_ok,
            version: docker_ver,
            required: false,
            post_install_note: Some("安装后 Cursor 的「Cursor 中打开」按钮可直接连进远端 Docker 容器".into()),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_dependency_status() -> Vec<DepInfo> {
    tokio::task::spawn_blocking(check_all)
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub async fn install_dependency(kind: DepKind) -> InstallResult {
    let post_note = check_all()
        .into_iter()
        .find(|d| d.kind == kind)
        .and_then(|d| d.post_install_note.clone());

    let kind_clone = kind.clone();
    let (success, output) = tokio::task::spawn_blocking(move || match kind_clone {
        DepKind::Homebrew   => install_homebrew(),
        DepKind::Tmux       => brew_install("tmux", false),
        DepKind::Claude     => brew_install("claude", false),
        DepKind::DockerCli  => brew_install("docker", false),
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
    cmd.env("HOMEBREW_NO_ENV_HINTS", "1")
       .env("NONINTERACTIVE", "1"); // suppress interactive prompts
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

fn install_homebrew() -> (bool, String) {
    let script = "/bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"";
    let result = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(r#"tell application "Terminal" to do script "{script}""#),
        ])
        .output();
    match result {
        Ok(o) if o.status.success() => (
            true,
            "已在 Terminal 中打开安装脚本，请在 Terminal 中完成安装后回来刷新".into(),
        ),
        _ => (
            false,
            format!("请手动在 Terminal 运行：\n{script}"),
        ),
    }
}
