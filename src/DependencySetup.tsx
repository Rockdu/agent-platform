import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface DepInfo {
  kind: string;
  label: string;
  description: string;
  installed: boolean;
  version: string | null;
  required: boolean;
  postInstallNote: string | null;
}

interface InstallResult {
  kind: string;
  success: boolean;
  output: string;
  postInstallNote: string | null;
}

async function getDependencyStatus(): Promise<DepInfo[]> {
  return invoke<DepInfo[]>("get_dependency_status");
}

async function installDependency(kind: string): Promise<InstallResult> {
  return invoke<InstallResult>("install_dependency", { kind: kindToRust(kind) });
}

// Map camelCase frontend kind → Rust enum variant name
function kindToRust(kind: string): string {
  const map: Record<string, string> = {
    Homebrew: "Homebrew",
    Claude: "Claude",
    Tmux: "Tmux",
    DockerCli: "DockerCli",
    MacFuse: "MacFuse",
    Sshfs: "Sshfs",
  };
  return map[kind] ?? kind;
}

export function DependencySetupView({ onAllInstalled }: { onAllInstalled?: () => void }) {
  const [deps, setDeps] = useState<DepInfo[]>([]);
  const [installing, setInstalling] = useState<string | null>(null);
  const [results, setResults] = useState<Record<string, InstallResult>>({});
  const [loading, setLoading] = useState(true);
  const onAllInstalledRef = useRef(onAllInstalled);
  onAllInstalledRef.current = onAllInstalled;

  const refresh = useCallback(async () => {
    const list = await getDependencyStatus();
    setDeps(list);
    setLoading(false);
    if (list.every((d) => !d.required || d.installed)) {
      onAllInstalledRef.current?.();
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Auto-poll every 3 s while an install is in progress so the
  // status updates without needing a tab switch (e.g. cask installs
  // that run in Terminal; we detect completion via polling).
  useEffect(() => {
    if (installing === null) return;
    const id = setInterval(() => { void refresh(); }, 3000);
    return () => clearInterval(id);
  }, [installing, refresh]);

  const install = useCallback(
    async (kind: string) => {
      setInstalling(kind);
      try {
        const result = await installDependency(kind);
        setResults((prev) => ({ ...prev, [kind]: result }));
        await refresh();
      } finally {
        setInstalling(null);
      }
    },
    [refresh],
  );

  // installAll runs a fresh sequential install queue, bypassing the
  // shared `install` helper so setInstalling never races with the loop.
  const installAll = useCallback(async () => {
    // Capture the list at click time (before any installs).
    const queue = deps.filter((d) => !d.installed).map((d) => d.kind);
    if (queue.length === 0) return;
    for (const kind of queue) {
      setInstalling(kind);
      try {
        const result = await installDependency(kind);
        setResults((prev) => ({ ...prev, [kind]: result }));
        // Refresh dep status after each install so subsequent deps
        // (e.g. sshfs after macFUSE) see the updated state.
        const updated = await getDependencyStatus();
        setDeps(updated);
      } catch (err) {
        setResults((prev) => ({
          ...prev,
          [kind]: {
            kind,
            success: false,
            output: String(err),
            postInstallNote: null,
          },
        }));
      }
    }
    setInstalling(null);
    // Final refresh to sync version strings and overall status.
    await refresh();
  }, [deps, refresh]);

  if (loading) {
    return <div className="dep-setup__loading">检查依赖中…</div>;
  }

  const allOk = deps.every((d) => d.installed);
  const requiredMissing = deps.filter((d) => d.required && !d.installed);
  const anyMissing = deps.some((d) => !d.installed);

  return (
    <div className="dep-setup">
      <header className="dep-setup__header">
        <h2>环境依赖</h2>
        <p className="dep-setup__subtitle">
          {allOk
            ? "所有依赖已安装 ✓"
            : requiredMissing.length > 0
              ? `缺少 ${requiredMissing.length} 个必要依赖`
              : "可选依赖未安装（部分功能不可用）"}
        </p>
      </header>

      <ul className="dep-setup__list">
        {deps.map((dep) => {
          const result = results[dep.kind];
          const busy = installing === dep.kind;
          return (
            <li
              key={dep.kind}
              className={`dep-setup__item ${dep.installed ? "dep-setup__item--ok" : dep.required ? "dep-setup__item--missing" : "dep-setup__item--optional"}`}
            >
              <div className="dep-setup__item-left">
                <span className="dep-setup__status">
                  {dep.installed ? "✓" : dep.required ? "✗" : "–"}
                </span>
                <div>
                  <div className="dep-setup__name">
                    {dep.label}
                    {dep.required && (
                      <span className="dep-setup__badge dep-setup__badge--required">
                        必须
                      </span>
                    )}
                    {!dep.required && (
                      <span className="dep-setup__badge dep-setup__badge--optional">
                        可选
                      </span>
                    )}
                  </div>
                  <div className="dep-setup__desc">{dep.description}</div>
                  {dep.version && (
                    <div className="dep-setup__version">{dep.version}</div>
                  )}
                  {result && (
                    <pre className="dep-setup__output">{result.output}</pre>
                  )}
                  {result?.postInstallNote && (
                    <div className="dep-setup__note">
                      ⚠ {result.postInstallNote}
                    </div>
                  )}
                </div>
              </div>
              {!dep.installed && (
                <button
                  type="button"
                  className="dep-setup__install-btn"
                  onClick={() => void install(dep.kind)}
                  disabled={busy || installing !== null}
                >
                  {busy ? "安装中…" : "安装"}
                </button>
              )}
            </li>
          );
        })}
      </ul>

      <footer className="dep-setup__footer">
        <div className="dep-setup__footer-row">
          {anyMissing && (
            <button
              type="button"
              className="dep-setup__install-all-btn"
              onClick={() => void installAll()}
              disabled={installing !== null}
            >
              {installing !== null
                ? `正在安装：${installing}…`
                : "一键安装全部缺少的依赖"}
            </button>
          )}
          <button
            type="button"
            className="dep-setup__refresh-btn"
            onClick={() => void refresh()}
            disabled={loading}
          >
            ↻ 刷新状态
          </button>
        </div>
        {anyMissing && (
          <p className="dep-setup__hint">
            FUSE-T 安装时会打开 Terminal 窗口，在里面完成后点「刷新状态」确认。
            其余依赖在后台安装，每 3 秒自动刷新。
          </p>
        )}
      </footer>
    </div>
  );
}
