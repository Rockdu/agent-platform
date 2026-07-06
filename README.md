# Agent Platform

A macOS desktop app that brings your **terminals, AI orchestrator agent, and papers/notes** together in one window. Built with Tauri v2 + Rust + React.

> 每个工作区一套隔离终端，一个特权 orchestrator 标签页自动拉起 `claude` 作为编排 agent，插件以独立进程（MCP sidecar）运行，所有对外写操作都经过确认弹窗。

---

## 🚀 怎么跑（一条命令）

macOS，克隆下来后直接：

```bash
./scripts/setup.sh --with-claude --run
```

这一条命令会**自动做完所有事**：装依赖 → 编译 → 装 `claude` CLI → 登录 → 启动 app。
脚本是幂等的，重复跑也没问题。

> ⚠️ **在你自己的终端里跑**：登录那步（`claude auth login`）会弹出浏览器让你授权，需要真实终端环境。

跑完窗口就打开了。想分步来，看下面的参数。

### 常用参数

```bash
./scripts/setup.sh                 # 只装依赖 + 编译
./scripts/setup.sh --with-claude   # 顺便装 Claude Code CLI
./scripts/setup.sh --skip-login    # 不自动触发 claude 登录
./scripts/setup.sh --run           # 编译完直接启动 app（开发模式）
./scripts/setup.sh --help          # 查看全部说明
```

日常开发还有更细的命令：`./build.sh help`（`dev` / `web` / `build` / `app` / `test` / `clippy` / `clean` …）。

### 需要准备什么

脚本能装的都会帮你装，装不了的会提示你：

| 依赖 | 说明 |
|---|---|
| **Xcode Command Line Tools** | 必需（C 工具链）。缺了脚本会提示你装。 |
| **Node.js 20+ / npm** | 必需。缺了用 `brew install node`。 |
| **Rust (stable)** | 缺 `cargo` 时脚本用 [rustup](https://rustup.rs) 自动装。 |
| **Claude Code CLI** (`claude`) | app 的核心 agent。加 `--with-claude` 自动装并登录。 |

---

## 🖥️ 跑起来之后怎么用

- **登录 claude**：首次启动前脚本会引导你 `claude auth login`（浏览器授权）。
  也可以设 `ANTHROPIC_API_KEY` 用 API key 免登录。用 `claude auth status` 查看状态。
- **orchestrator 标签页**（左上角，唯一）：自动拉起 `claude`，是你和 agent 对话、
  下达任务的地方。没登录 / 没装 claude 时这里会显示引导卡片。
- **Terminal Mesh**：每个工作区一组隔离的终端（PTY）。
- **Papers**：Zotero + arXiv 论文管理。**Notes**：笔记。
- **工作区**：真实目录，位于 `~/AgentPlatform/workspaces/<名字>/`，可以用
  Cursor / VS Code 打开做代码审查。
- **在 IDE 里打开**：默认用 Cursor，可在应用内改成 VS Code（`code`）、Zed（`zed`）
  或任意命令——一个都没装也不影响 app 运行，只是这个按钮用不了。
- **写操作确认**：agent 经插件发起的每次写入，都会弹确认框由你放行。

---

## 🔧 常见问题

- **`cargo` / `claude` 找不到？** 新开一个终端（让 `~/.cargo/bin`、
  `$(npm prefix -g)/bin` 进 PATH），或直接重跑 `./scripts/setup.sh`。
- **`npm install` 报安装脚本被拦（npm 11+）？** esbuild / fsevents 的白名单已写进
  `package.json` 的 `allowScripts`，正常 `npm install` 即可；装 claude 时脚本用了
  `--allow-scripts=@anthropic-ai/claude-code`。
- **编译报 "C compiler cannot create executables"（libsodium）？** 并行编译偶发竞态，
  重跑一次即可——`setup.sh` 已内置自动清理 + 重试。
- **orchestrator 是空的 / 提示找不到 claude？** 说明没装或没登录：
  `./scripts/setup.sh --with-claude` 然后 `claude auth login`，重启 app。

---

## 手动安装（脚本背后做的事）

不想用脚本、想一步步来：

```bash
# 1. Rust 工具链（缺 cargo 时）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# 2. 前端依赖（esbuild/fsevents 白名单已在 package.json 里）
npm install

# 3. Rust 代码生成 + 插件 sidecar + 前端打包 / 启动
npm run build          # tsc + vite build（prebuild 会跑 codegen 和 sidecar）
npm run tauri dev      # 或编译并启动 app

# 4. Claude Code CLI（核心 agent）+ 登录
npm install -g --allow-scripts=@anthropic-ai/claude-code @anthropic-ai/claude-code
claude auth login      # 浏览器 OAuth；或设 ANTHROPIC_API_KEY 用 API key
```

---

> 架构设计与开发文档见 [`docs/`](docs/)。
