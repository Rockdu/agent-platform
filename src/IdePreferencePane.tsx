import { useCallback, useEffect, useState } from "react";
import {
  getIdePreference,
  isIdeHandoffErrorDto,
  setIdePreference,
  type IdeHandoffErrorDto,
  type IdePreference,
} from "./ide-handoff";

export interface IdePreferencePaneProps {
  /// Called whenever the pane's actions raise a typed
  /// `IdeHandoffErrorDto`, OR with `null` after a successful save
  /// to clear any previous error. The host surface (modal /
  /// per-tab settings panel) owns the error display.
  onError: (e: IdeHandoffErrorDto | null) => void;
}

/// Canonical IDE preference editor. Shared between the workspace
/// switcher modal's `设置` tab and the per-active-tab workspace
/// settings panel so both surfaces stay in sync without duplicating
/// state.
export function IdePreferencePane({ onError }: IdePreferencePaneProps) {
  const [pref, setPref] = useState<IdePreference | null>(null);
  const [commandDraft, setCommandDraft] = useState("");
  const [argsDraft, setArgsDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        const p = await getIdePreference();
        setPref(p);
        setCommandDraft(p.ideCommand);
        setArgsDraft(p.ideArgsTemplate.join(","));
      } catch (err) {
        if (isIdeHandoffErrorDto(err)) onError(err);
      }
    })();
  }, [onError]);

  const onSave = useCallback(async () => {
    setBusy(true);
    setStatus(null);
    try {
      const updated = await setIdePreference({
        ideCommand: commandDraft.trim(),
        ideArgsTemplate: argsDraft
          .split(",")
          .map((s) => s.trim())
          .filter((s) => s !== ""),
      });
      setPref(updated);
      setStatus("已保存");
      onError(null);
    } catch (err) {
      if (isIdeHandoffErrorDto(err)) onError(err);
    } finally {
      setBusy(false);
    }
  }, [commandDraft, argsDraft, onError]);

  if (!pref) {
    return <p className="workspace-switcher-modal__hint">加载中…</p>;
  }
  return (
    <div className="workspace-switcher-modal__settings">
      <p className="workspace-switcher-modal__hint">
        “Cursor 中打开” 会调用以下命令并附加工作区路径。可改为 <code>code</code>、
        <code>zed</code> 或其它 IDE 的 CLI。
      </p>
      <label className="workspace-switcher-modal__field">
        IDE 命令
        <input
          type="text"
          value={commandDraft}
          onChange={(e) => setCommandDraft(e.target.value)}
          spellCheck={false}
        />
      </label>
      <label className="workspace-switcher-modal__field">
        参数模板（用逗号分隔，<code>{"{path}"}</code> 会被替换为工作区路径）
        <input
          type="text"
          value={argsDraft}
          onChange={(e) => setArgsDraft(e.target.value)}
          spellCheck={false}
        />
      </label>
      <div className="workspace-switcher-modal__actions">
        <button
          type="button"
          onClick={() => void onSave()}
          disabled={busy || commandDraft.trim() === ""}
        >
          保存
        </button>
        {status && (
          <span className="workspace-switcher-modal__hint">{status}</span>
        )}
      </div>
    </div>
  );
}
