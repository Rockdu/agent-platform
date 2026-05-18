import { useCallback, useEffect, useState } from "react";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  createWorkspace,
  isWorkspaceErrorDto,
  localPath,
  registerRemoteWorkspace,
  registerWorkspace,
  type WorkspaceErrorDto,
  type WorkspaceRecord,
} from "./workspaces";
import {
  isIdeHandoffErrorDto,
  openWorkspaceInIde,
  revealWorkspaceInFinder,
  type IdeHandoffErrorDto,
} from "./ide-handoff";
import { IdePreferencePane } from "./IdePreferencePane";

type SwitcherMode = "create" | "recent" | "settings";

type CreateSource =
  | { kind: "auto" }
  | { kind: "existing"; pickedPath: string | null }
  | {
      kind: "remote-ssh";
      host: string;
      user: string;
      port: string;
      canonicalRemotePath: string;
      containerEnabled: boolean;
      containerId: string;
      cwdInContainer: string;
    };

function freshRemoteSource(): CreateSource {
  return {
    kind: "remote-ssh",
    host: "",
    user: "",
    port: "",
    canonicalRemotePath: "",
    containerEnabled: false,
    containerId: "",
    cwdInContainer: "",
  };
}

export interface WorkspaceSwitcherModalProps {
  open: boolean;
  workspaces: WorkspaceRecord[];
  /// IDs of workspaces currently open in the local session — used to
  /// label rows and decide between "focus" vs "open" on click.
  openWorkspaceIds: Set<string>;
  /// Called when the user picks an existing workspace from the
  /// recent list. The container decides whether to focus its
  /// existing tab or open a new one.
  onPickExisting: (workspace: WorkspaceRecord) => Promise<void> | void;
  /// Called after a successful `createWorkspace`/`registerWorkspace`
  /// so the container can immediately open the freshly created
  /// workspace as a tab. The container is responsible for the
  /// `openWorkspace` call that follows.
  onAdopt: (workspace: WorkspaceRecord) => Promise<void> | void;
  onClose: () => void;
}

export function WorkspaceSwitcherModal({
  open,
  workspaces,
  openWorkspaceIds,
  onPickExisting,
  onAdopt,
  onClose,
}: WorkspaceSwitcherModalProps) {
  const [mode, setMode] = useState<SwitcherMode>("create");
  const [name, setName] = useState("");
  const [source, setSource] = useState<CreateSource>({ kind: "auto" });
  const [error, setError] = useState<WorkspaceErrorDto | null>(null);
  const [busy, setBusy] = useState(false);
  const [ideError, setIdeError] = useState<IdeHandoffErrorDto | null>(null);
  // Per the product spec the auto-launch checkbox defaults CHECKED.
  // Persisted into WorkspaceProfile.auto_launch_claude on the chosen
  // create/register path.
  const [autoLaunchClaude, setAutoLaunchClaude] = useState(true);

  const onCursor = useCallback(async (workspacePath: string) => {
    setIdeError(null);
    try {
      await openWorkspaceInIde(workspacePath);
    } catch (err) {
      if (isIdeHandoffErrorDto(err)) setIdeError(err);
    }
  }, []);

  const onFinder = useCallback(async (workspacePath: string) => {
    setIdeError(null);
    try {
      await revealWorkspaceInFinder(workspacePath);
    } catch (err) {
      if (isIdeHandoffErrorDto(err)) setIdeError(err);
    }
  }, []);

  // Esc-key close. Bound only while the modal is open so other panes
  // keep their own Esc handlers intact.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  // Reset transient state when the modal closes so the next open
  // starts fresh.
  useEffect(() => {
    if (!open) {
      setName("");
      setSource({ kind: "auto" });
      setError(null);
      setBusy(false);
      setMode("create");
    }
  }, [open]);

  const onPickDirectory = useCallback(async () => {
    try {
      const picked = await openFileDialog({
        multiple: false,
        directory: true,
        title: "选择已有工作区目录",
      });
      if (picked === null) {
        setSource({ kind: "existing", pickedPath: null });
        return;
      }
      const p = Array.isArray(picked) ? picked[0] : picked;
      if (typeof p !== "string" || p === "") {
        setSource({ kind: "existing", pickedPath: null });
        return;
      }
      setSource({ kind: "existing", pickedPath: p });
    } catch (err) {
      if (isWorkspaceErrorDto(err)) setError(err);
    }
  }, []);

  const onConfirmCreate = useCallback(async () => {
    setError(null);
    setBusy(true);
    try {
      let created: WorkspaceRecord | null = null;
      if (source.kind === "auto") {
        const trimmed = name.trim();
        if (!trimmed) {
          setError({ kind: "invalidName", reason: "请输入工作区名称" });
          return;
        }
        created = await createWorkspace(trimmed, autoLaunchClaude);
      } else if (source.kind === "existing") {
        if (!source.pickedPath) {
          setError({
            kind: "invalidName",
            reason: "请先选择一个已有目录",
          });
          return;
        }
        created = await registerWorkspace(source.pickedPath, autoLaunchClaude);
      } else {
        // Remote SSH workspace.
        const trimmedName = name.trim();
        if (!trimmedName) {
          setError({ kind: "invalidName", reason: "请输入工作区名称" });
          return;
        }
        let port: number | null = null;
        if (source.port.trim() !== "") {
          const parsed = Number.parseInt(source.port.trim(), 10);
          if (!Number.isFinite(parsed) || parsed < 1 || parsed > 65535) {
            setError({
              kind: "remoteFieldInvalid",
              field: "port",
              reason: "端口需在 1..=65535 范围内",
            });
            return;
          }
          port = parsed;
        }
        const userTrimmed = source.user.trim();
        const containerId = source.containerEnabled
          ? source.containerId.trim()
          : "";
        const cwdInContainerTrimmed = source.containerEnabled
          ? source.cwdInContainer.trim()
          : "";
        created = await registerRemoteWorkspace({
          name: trimmedName,
          host: source.host.trim(),
          user: userTrimmed === "" ? null : userTrimmed,
          port,
          canonicalRemotePath: source.canonicalRemotePath.trim(),
          containerId: source.containerEnabled ? containerId : null,
          cwdInContainer:
            source.containerEnabled && cwdInContainerTrimmed !== ""
              ? cwdInContainerTrimmed
              : null,
          autoLaunchClaude,
        });
      }
      if (created) {
        await onAdopt(created);
        onClose();
      }
    } catch (err) {
      if (isWorkspaceErrorDto(err)) setError(err);
    } finally {
      setBusy(false);
    }
  }, [name, source, onAdopt, onClose, autoLaunchClaude]);

  const onRowClick = useCallback(
    async (w: WorkspaceRecord) => {
      setBusy(true);
      try {
        await onPickExisting(w);
        onClose();
      } catch (err) {
        if (isWorkspaceErrorDto(err)) setError(err);
      } finally {
        setBusy(false);
      }
    },
    [onPickExisting, onClose],
  );

  if (!open) return null;

  return (
    <div
      className="modal-overlay"
      role="presentation"
      onClick={(e) => {
        // Click on the dim overlay (outside the card) cancels.
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="modal-card workspace-switcher-modal"
        role="dialog"
        aria-modal="true"
        aria-label="工作区"
      >
        <header className="workspace-switcher-modal__header">
          <h2>工作区</h2>
          <button
            type="button"
            className="workspace-switcher-modal__close"
            onClick={onClose}
            aria-label="关闭"
          >
            ×
          </button>
        </header>
        <nav className="workspace-switcher-modal__tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "create"}
            className={`workspace-switcher-modal__tab ${
              mode === "create" ? "workspace-switcher-modal__tab--active" : ""
            }`}
            onClick={() => setMode("create")}
          >
            新建
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "recent"}
            className={`workspace-switcher-modal__tab ${
              mode === "recent" ? "workspace-switcher-modal__tab--active" : ""
            }`}
            onClick={() => setMode("recent")}
          >
            最近使用 ({workspaces.length})
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "settings"}
            className={`workspace-switcher-modal__tab ${
              mode === "settings" ? "workspace-switcher-modal__tab--active" : ""
            }`}
            onClick={() => setMode("settings")}
          >
            设置
          </button>
        </nav>
        <div className="workspace-switcher-modal__body">
          {mode === "create" && (
            <CreatePane
              name={name}
              setName={setName}
              source={source}
              setSource={setSource}
              onPickDirectory={onPickDirectory}
              onConfirm={onConfirmCreate}
              onCancel={onClose}
              busy={busy}
              autoLaunchClaude={autoLaunchClaude}
              setAutoLaunchClaude={setAutoLaunchClaude}
            />
          )}
          {mode === "recent" && (
            <RecentPane
              workspaces={workspaces}
              openWorkspaceIds={openWorkspaceIds}
              onRowClick={onRowClick}
              onCursor={onCursor}
              onFinder={onFinder}
              busy={busy}
            />
          )}
          {mode === "settings" && <IdePreferencePane onError={setIdeError} />}
        </div>
        {ideError && (
          <p
            className="workspace-switcher-modal__error"
            role="alert"
            data-ide-handoff-error={ideError.kind}
          >
            <code>{ideError.kind}</code>: {renderIdeError(ideError)}
          </p>
        )}
        {error && (
          <p
            className="workspace-switcher-modal__error"
            role="alert"
            data-workspace-switcher-error={error.kind}
          >
            <code>{error.kind}</code>: {renderErrorMessage(error)}
          </p>
        )}
      </div>
    </div>
  );
}

function CreatePane(props: {
  name: string;
  setName: (s: string) => void;
  source: CreateSource;
  setSource: (s: CreateSource) => void;
  onPickDirectory: () => Promise<void> | void;
  onConfirm: () => Promise<void> | void;
  onCancel: () => void;
  busy: boolean;
  autoLaunchClaude: boolean;
  setAutoLaunchClaude: (v: boolean) => void;
}) {
  const {
    name,
    setName,
    source,
    setSource,
    onPickDirectory,
    onConfirm,
    onCancel,
    busy,
    autoLaunchClaude,
    setAutoLaunchClaude,
  } = props;
  return (
    <form
      className="workspace-switcher-modal__create"
      onSubmit={(e) => {
        e.preventDefault();
        void onConfirm();
      }}
    >
      <div className="workspace-switcher-modal__field">
        <label>
          <input
            type="radio"
            name="source"
            checked={source.kind === "auto"}
            onChange={() => setSource({ kind: "auto" })}
          />{" "}
          自动创建在 <code>~/AgentPlatform/workspaces/&lt;name&gt;/</code>
        </label>
        <label>
          <input
            type="radio"
            name="source"
            checked={source.kind === "existing"}
            onChange={() => setSource({ kind: "existing", pickedPath: null })}
          />{" "}
          选择已有目录
        </label>
        <label>
          <input
            type="radio"
            name="source"
            checked={source.kind === "remote-ssh"}
            onChange={() => setSource(freshRemoteSource())}
          />{" "}
          远程 SSH 工作区
        </label>
      </div>
      {source.kind === "auto" && (
        <label className="workspace-switcher-modal__field">
          名称
          <input
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="项目名 (字母、数字、汉字、-、_、.、空格,≤ 64)"
            spellCheck={false}
            autoFocus
          />
        </label>
      )}
      {source.kind === "existing" && (
        <div className="workspace-switcher-modal__field">
          <button
            type="button"
            onClick={() => void onPickDirectory()}
            disabled={busy}
          >
            选择目录…
          </button>
          {source.pickedPath ? (
            <p>
              即将注册: <code>{source.pickedPath}</code>
            </p>
          ) : (
            <p className="workspace-switcher-modal__hint">
              尚未选择目录;点击上方按钮挑选。
            </p>
          )}
        </div>
      )}
      {source.kind === "remote-ssh" && (
        <div className="workspace-switcher-modal__field workspace-switcher-modal__remote">
          <label>
            名称
            <input
              type="text"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="项目名 (字母、数字、汉字、-、_、.、空格,≤ 64)"
              spellCheck={false}
            />
          </label>
          <label>
            主机
            <input
              type="text"
              value={source.host}
              onChange={(e) => setSource({ ...source, host: e.target.value })}
              placeholder="host.example.com (必填)"
              spellCheck={false}
            />
          </label>
          <label>
            用户（可选）
            <input
              type="text"
              value={source.user}
              onChange={(e) => setSource({ ...source, user: e.target.value })}
              placeholder="默认使用本地用户名"
              spellCheck={false}
            />
          </label>
          <label>
            端口（可选）
            <input
              type="text"
              inputMode="numeric"
              value={source.port}
              onChange={(e) => setSource({ ...source, port: e.target.value })}
              placeholder="默认 22"
            />
          </label>
          <label>
            远程工作目录
            <input
              type="text"
              value={source.canonicalRemotePath}
              onChange={(e) =>
                setSource({ ...source, canonicalRemotePath: e.target.value })
              }
              placeholder="/home/me/repo (必填)"
              spellCheck={false}
            />
          </label>
          <label>
            <input
              type="checkbox"
              checked={source.containerEnabled}
              onChange={(e) =>
                setSource({ ...source, containerEnabled: e.target.checked })
              }
            />{" "}
            在已有 Docker 容器内运行（仅支持现存容器，不会创建/启动/停止容器）
          </label>
          {source.containerEnabled && (
            <>
              <label>
                容器 ID
                <input
                  type="text"
                  value={source.containerId}
                  onChange={(e) =>
                    setSource({ ...source, containerId: e.target.value })
                  }
                  placeholder="必填"
                  spellCheck={false}
                />
              </label>
              <label>
                容器内工作目录（可选）
                <input
                  type="text"
                  value={source.cwdInContainer}
                  onChange={(e) =>
                    setSource({ ...source, cwdInContainer: e.target.value })
                  }
                  placeholder="默认沿用远程工作目录"
                  spellCheck={false}
                />
              </label>
            </>
          )}
          <p className="workspace-switcher-modal__hint">
            远程 claude 仅以 shell 形式运行；本应用的 MCP 插件不会注入到远程 claude（v1 范围，DEC-9）。
          </p>
        </div>
      )}
      <label className="workspace-switcher-modal__field workspace-switcher-modal__auto-launch">
        <input
          type="checkbox"
          checked={autoLaunchClaude}
          onChange={(e) => setAutoLaunchClaude(e.target.checked)}
          disabled={busy}
        />{" "}
        创建后自动启动 claude（带 <code>--dangerously-skip-permissions</code>）
      </label>
      <div className="workspace-switcher-modal__actions">
        <button type="button" onClick={onCancel} disabled={busy}>
          取消
        </button>
        <button
          type="submit"
          disabled={
            busy ||
            (source.kind === "auto" && name.trim() === "") ||
            (source.kind === "existing" && !source.pickedPath) ||
            (source.kind === "remote-ssh" &&
              (name.trim() === "" ||
                source.host.trim() === "" ||
                source.canonicalRemotePath.trim() === "" ||
                (source.containerEnabled && source.containerId.trim() === "")))
          }
        >
          创建
        </button>
      </div>
    </form>
  );
}

function renderRemoteLocation(w: WorkspaceRecord): string {
  if (w.location.kind !== "remote") return "";
  const { ssh, container } = w.location;
  const user = ssh.user ? `${ssh.user}@` : "";
  const port = ssh.port ? `:${ssh.port}` : "";
  const base = `ssh://${user}${ssh.host}${port}${ssh.canonicalRemotePath}`;
  return container
    ? `${base} (container ${container.containerId})`
    : base;
}

function RecentPane(props: {
  workspaces: WorkspaceRecord[];
  openWorkspaceIds: Set<string>;
  onRowClick: (w: WorkspaceRecord) => Promise<void> | void;
  onCursor: (path: string) => Promise<void> | void;
  onFinder: (path: string) => Promise<void> | void;
  busy: boolean;
}) {
  const { workspaces, openWorkspaceIds, onRowClick, onCursor, onFinder, busy } = props;
  if (workspaces.length === 0) {
    return (
      <p className="workspace-switcher-modal__hint">
        还没有任何工作区。切换到 “新建” 创建第一个。
      </p>
    );
  }
  return (
    <ul className="workspace-switcher-modal__list">
      {workspaces.map((w) => (
        <li key={w.workspaceId}>
          <div
            className="workspace-switcher-modal__row workspace-switcher-modal__row--composite"
            data-workspace-id={w.workspaceId}
            data-open={openWorkspaceIds.has(w.workspaceId) ? "true" : "false"}
          >
            <button
              type="button"
              className="workspace-switcher-modal__row-main"
              onClick={() => void onRowClick(w)}
              disabled={busy}
            >
              <div className="workspace-switcher-modal__row-name">
                <strong>{w.name}</strong>
                {openWorkspaceIds.has(w.workspaceId) && (
                  <span className="workspace-switcher-modal__row-badge">
                    已打开
                  </span>
                )}
              </div>
              <code className="workspace-switcher-modal__row-path">
                {localPath(w) ?? renderRemoteLocation(w)}
              </code>
              <div className="workspace-switcher-modal__row-meta">
                <span>最近使用: {w.lastUsedAt}</span>
                <span>创建: {w.createdAt}</span>
                <span>
                  claude 对话:{" "}
                  {w.conversationRoundsCount > 0
                    ? `${w.conversationRoundsCount} 轮`
                    : "无"}
                </span>
              </div>
            </button>
            <div className="workspace-switcher-modal__row-actions">
              {(() => {
                const p = localPath(w);
                return (
                  <>
                    <button
                      type="button"
                      className="workspace-switcher-modal__row-action"
                      onClick={(e) => {
                        // Row-level action buttons must not also
                        // trigger the surrounding row's open handler.
                        e.stopPropagation();
                        if (p) void onCursor(p);
                      }}
                      disabled={busy || p === null}
                      title={p === null ? "远程工作区暂不支持 Cursor" : undefined}
                    >
                      Cursor 中打开
                    </button>
                    <button
                      type="button"
                      className="workspace-switcher-modal__row-action"
                      onClick={(e) => {
                        e.stopPropagation();
                        if (p) void onFinder(p);
                      }}
                      disabled={busy || p === null}
                      title={p === null ? "远程工作区暂不支持 Finder" : undefined}
                    >
                      Finder 中显示
                    </button>
                  </>
                );
              })()}
            </div>
          </div>
        </li>
      ))}
    </ul>
  );
}

function renderIdeError(e: IdeHandoffErrorDto): string {
  switch (e.kind) {
    case "ideNotInPath":
      return `命令 ${e.command} 不在 PATH 中。请检查 IDE 是否已安装并加入 PATH。`;
    case "notADirectory":
      return `不是有效目录: ${e.path}`;
    case "spawnFailed":
      return `启动 ${e.command} 失败: ${e.message}`;
    case "io":
      return `IO 错误 (${e.context}): ${e.message}`;
  }
}

function renderErrorMessage(error: WorkspaceErrorDto): string {
  switch (error.kind) {
    case "invalidName":
      return error.reason;
    case "workspaceAlreadyExists":
      return `目录已存在: ${error.path}`;
    case "canonicalDuplicate":
      return `已存在同一目录的工作区: ${error.existingName}`;
    case "notADirectory":
      return `不是有效的目录: ${error.path}`;
    case "notFound":
      return `工作区不存在: ${error.workspaceId}`;
    case "alreadyOpen":
      return `该工作区已经在其他 tab 中打开: ${error.existingTabId}`;
    case "io":
      return `IO 错误 (${error.context}): ${error.message}`;
    case "remoteFieldInvalid":
      return `远程字段 ${error.field} 无效: ${error.reason}`;
    case "remoteProbeFailed":
      return `远程连接预检失败 (${error.phase}): ${error.reason}`;
  }
}
