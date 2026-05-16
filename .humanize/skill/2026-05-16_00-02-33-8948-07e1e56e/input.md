# Ask Codex Input

## Question

You are doing a first-pass planning critique for a greenfield software project. This is BEFORE Claude synthesizes its candidate plan — your role is to surface assumptions, missing requirements, technical gaps, alternative directions, user-decision questions, and candidate acceptance criteria so Claude can build a stronger plan.

# Repository Context

- Path: `/Users/rockdu/claude_workspace`
- State: GREENFIELD — `git init`ed today, only contains `.humanize/` (idea draft + memory metadata) and `.git/`. No README, no CLAUDE.md, no source code yet.
- Project purpose: a personal-agent platform for information management (Gmail multi-account, Zotero+arXiv) and agent terminal management (multiple xterm.js terminals with completion/needs-attention notifications surfaced through a macOS menubar tray window as a "灵动岛"-style alert). Extensibility for future tabs (GitHub PR review, WeChat, etc.) is a first-class requirement.

# Locked Decisions (DO NOT propose changes to these)

1. **Stack is locked**: Tauri v2 (Rust + native WebView) + React/TypeScript frontend + Rust adapters. Do NOT propose Electron, pure-web SPA, SwiftUI/native-Apple, Notebook-cell, or tmux-multiplexer alternatives — those alternatives were already evaluated and rejected.
2. **MVP scope is locked**: three tabs only — Terminal Mesh (xterm.js + portable-pty), Gmail multi-account (google-gmail1 + per-account OAuth, or rust-imap as fallback), Zotero + arXiv papers (Zotero local SQLite/HTTP API + arxiv-rs).
3. **Deferred to phase 2**: WeChat integration (compliance + reverse-engineering risk under review), GitHub PR review tab.
4. **Notification surface**: tauri-plugin-notification + tauri-plugin-positioner menubar-tray window pinned `TrayBottomCenter` is the agreed macOS desktop analog for Dynamic Island. No iOS companion app in MVP.
5. **Persistence**: SQLite (rusqlite) per-plugin keyed state. OAuth tokens via OS keychain (tauri-plugin-stronghold).

Your critique should ACCEPT these locked decisions as given and focus on what's underspecified WITHIN them.

# The Draft Being Critiqued

```
# Tauri Plugin-Hosted Personal Agent And Information Workspace

## Primary Direction: Tauri Plugin-Registry Workspace

### Approach Summary

A Tauri v2 desktop application paired with a modular plugin architecture serving as the unified information and agent-terminal platform. Frontend: React + TypeScript hosted in Tauri WebView with a shared design system. Tauri host (Rust): stable IPC command/event API (#[tauri::command] + tauri::emit/listen) that plugins implement against; lifecycle-aware plugin loading via a standardized manifest.

Plugin system: each integration is a self-contained pair — frontend component + Rust adapter — invoking Rust commands via tauri::invoke() and listening for events via tauri::listen(). Plugins declare permissions and required APIs in a manifest:

  {
    "name": "gmail",
    "type": "tab",
    "command": "tauri-plugin-gmail",
    "frontend": "@tauri-plugin-gmail-api",
    "permissions": ["user.email", "oauth2"],
    "requiredApis": ["invoke", "listen"]
  }

Sidecars: long-running services (PTY manager, email sync daemons, optional MCP-server bridges) spawn as Rust binaries via ShellExt::sidecar(). Terminal: xterm.js + portable-pty backend; per-terminal process state streams via tauri::emit() and mirrors to the notification layer. Notifications: tauri-plugin-notification for native macOS UNUserNotificationCenter; tauri-plugin-positioner for a menubar tray window pinned TrayBottomCenter. State: SQLite per plugin + OS keychain for OAuth tokens.

Initial tab set (MVP): Gmail multi-account, Zotero+arXiv, Terminal Mesh.

### Known Risks (from the draft)
- Multi-account Gmail token isolation
- Zotero version compatibility (SQLite vs HTTP API)
- Plugin isolation limits (no per-extension process sandboxing in Tauri)
- Terminal state sync latency
- "灵动岛" fidelity gap on macOS

### Original User Idea (verbatim)
> 我希望你在这个工作区里面开一个新仓库，这个仓库将要作为一个agent平台，担任我所有信息管理（邮件收发（连接gmail多个账号）、消息管理（连接微信）、论文管理和推荐（连接我的zotero和arxiv））、agent终端管理（我会开多个终端，我需要能实时查看各个终端的状态，并且某个终端工作完成了或者需要我操作了给我一个灵动岛提示）；并且需要支持增加更多的tab功能（比如后期我可能需要github仓库追踪和辅助pr code review功能等）；你要考虑到工程可维护性、前端美观度以及可扩展性
```

# Required Output Format

Return EXACTLY these six sections in this order, using these exact headings. Each item should be terse but specific. Do not invent additional sections.

## CORE_RISKS
- (highest-risk assumptions or potential failure modes specific to THIS Tauri MVP — not generic software risks)

## MISSING_REQUIREMENTS
- (requirements likely omitted from the draft — including edge cases the user probably wants but didn't state)

## TECHNICAL_GAPS
- (feasibility or architecture gaps WITHIN the locked stack — e.g., contracts that aren't defined yet, integration boundaries that are fuzzy, capabilities the chosen crates may not actually provide)

## ALTERNATIVE_DIRECTIONS
- (viable alternatives FOR SUB-COMPONENTS only — e.g., gmail crate choice, terminal backend, IPC schema — NOT for the overall stack which is locked. Each item: option + one-line tradeoff. If you have no sub-component alternatives to suggest, write `- None — locked decisions are sufficient at this granularity.`)

## QUESTIONS_FOR_USER
- (specific questions that need explicit human decisions before implementation can proceed. Phrase as questions ending with `?`)

## CANDIDATE_CRITERIA
- (candidate acceptance criteria for the MVP, phrased as testable conditions. Aim for 6–12 candidates spanning plugin contract, terminal mesh, gmail, papers tab, notifications, and persistence/secrets.)

Be honest. If a section has nothing material to say, say so explicitly rather than padding.

## Configuration

- Model: gpt-5.5
- Effort: high
- Timeout: 900s
- Timestamp: 2026-05-16_00-02-33
- Tool: codex
