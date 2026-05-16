import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

// Mirrors src-tauri/src/bootstrap.rs::BootstrapPaths
interface BootstrapPaths {
  agent_platform: string;
  workspaces: string;
  plugins_root: string;
  logs: string;
  claude_mcp_configs: string;
}

type BootstrapState =
  | { kind: "loading" }
  | { kind: "ok"; paths: BootstrapPaths }
  | { kind: "error"; message: string };

type TabId = "orchestrator" | "terminal-mesh" | "gmail" | "papers";

// Tab strip order: orchestrator is leftmost + fixed (per AC-3.1).
// The other three are MVP plugin tabs (Terminal Mesh, Gmail, Papers).
const TABS: { id: TabId; label: string; icon: string; privileged: boolean }[] = [
  { id: "orchestrator", label: "Orchestrator", icon: "✦", privileged: true },
  { id: "terminal-mesh", label: "终端", icon: "▤", privileged: false },
  { id: "gmail", label: "邮件", icon: "✉", privileged: false },
  { id: "papers", label: "论文", icon: "❡", privileged: false },
];

export default function App() {
  const [activeTab, setActiveTab] = useState<TabId>("orchestrator");
  const [bootstrap, setBootstrap] = useState<BootstrapState>({ kind: "loading" });

  useEffect(() => {
    invoke<BootstrapPaths>("bootstrap_status")
      .then((paths) => setBootstrap({ kind: "ok", paths }))
      .catch((err: unknown) =>
        setBootstrap({
          kind: "error",
          message: typeof err === "string" ? err : JSON.stringify(err),
        }),
      );
  }, []);

  return (
    <div className="app">
      <nav className="tab-strip" role="tablist">
        {TABS.map((tab) => (
          <button
            key={tab.id}
            role="tab"
            aria-selected={activeTab === tab.id}
            className={`tab ${tab.privileged ? "tab--privileged" : ""} ${
              activeTab === tab.id ? "tab--active" : ""
            }`}
            onClick={() => setActiveTab(tab.id)}
          >
            <span className="tab__icon" aria-hidden="true">
              {tab.icon}
            </span>
            <span className="tab__label">{tab.label}</span>
          </button>
        ))}
      </nav>

      <main className="tab-body">
        {activeTab === "orchestrator" && <OrchestratorPlaceholder />}
        {activeTab === "terminal-mesh" && <PluginPlaceholder name="终端 (Terminal Mesh)" />}
        {activeTab === "gmail" && <PluginPlaceholder name="邮件 (Gmail)" />}
        {activeTab === "papers" && <PluginPlaceholder name="论文 (Papers / Zotero+arXiv)" />}
      </main>

      <BootstrapDebugCard state={bootstrap} />
    </div>
  );
}

function OrchestratorPlaceholder() {
  return (
    <section className="placeholder">
      <h2>Orchestrator</h2>
      <p>
        Privileged single-instance tab. Will auto-launch <code>claude</code> with all platform MCP
        servers preconfigured (待 task13 / task20).
      </p>
      <p className="placeholder__hint">
        本轮（Round 0）仅渲染占位。task20+ 接入实际 PTY + claude wiring。
      </p>
    </section>
  );
}

function PluginPlaceholder({ name }: { name: string }) {
  return (
    <section className="placeholder">
      <h2>{name}</h2>
      <p>插件占位。task3 codegen + task5 SQLite + 各插件 sidecar 落地后会被实际插件实现替换。</p>
    </section>
  );
}

function BootstrapDebugCard({ state }: { state: BootstrapState }) {
  if (state.kind === "loading") {
    return (
      <aside className="bootstrap-card bootstrap-card--loading" aria-live="polite">
        <h3>启动中…</h3>
        <p>正在调用 <code>bootstrap_status</code> 校验首启目录。</p>
      </aside>
    );
  }
  if (state.kind === "error") {
    return (
      <aside className="bootstrap-card bootstrap-card--error" aria-live="polite">
        <h3>首启失败</h3>
        <p>{state.message}</p>
        <p className="bootstrap-card__hint">
          请检查上面路径是否可写。typed error 包含 offending path（参见后端 stderr 的 JSON 日志）。
        </p>
      </aside>
    );
  }
  return (
    <aside className="bootstrap-card bootstrap-card--ok" aria-live="polite">
      <h3>首启完成 (Round 0 debug)</h3>
      <dl>
        <dt>~/AgentPlatform</dt>
        <dd>{state.paths.agent_platform}</dd>
        <dt>workspaces</dt>
        <dd>{state.paths.workspaces}</dd>
        <dt>plugins root</dt>
        <dd>{state.paths.plugins_root}</dd>
        <dt>logs</dt>
        <dd>{state.paths.logs}</dd>
        <dt>claude-mcp-configs</dt>
        <dd>{state.paths.claude_mcp_configs}</dd>
      </dl>
      <p className="bootstrap-card__hint">
        此调试卡 task7/task8 落地后会被插件健康面板替换。
      </p>
    </aside>
  );
}
