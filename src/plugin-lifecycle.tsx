import {
  Component,
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ErrorInfo,
  type ReactNode,
} from "react";
import {
  mountPlugin,
  unmountPlugin,
  type PluginCapability,
} from "./generated/capability";

// Host-owned React lifecycle wrapper around the Rust dispatcher commands
// shipped in Round 5/6. Closes the React-side half of AC-1.5: the
// PluginCapability handle exists ONLY inside this component subtree's
// closure state + the per-plugin Context value. There is no window /
// localStorage / sessionStorage / IndexedDB serialization path; the brand
// type defined alongside the generated wrappers already prevents
// accidental cross-boundary smuggling at the type level.
//
// Race contract (mirrors the Rust dispatcher's MountRegistry invariants):
//
//   * onMount: issue capability via mountPlugin -> Rust mints a fresh
//     mount_id + nonce, returns the cap_v1.<mount_uuid>.<nonce_b64url>
//     handle. We hold it in React closure state only.
//   * onUnmount: revoke via unmountPlugin. Best-effort; we ignore the
//     result because the React render tree is already torn down.
//   * Remount-on-key: when pluginId or tabId changes, useEffect cleanup
//     unmounts the old capability before the next effect mints a new one.
//   * In-flight IPC during unmount: the Rust dispatcher returns
//     CapabilityExpired once the registry entry is gone. The wrapper
//     does NOT auto-retry stale handles; the plugin error boundary
//     surfaces the typed failure for plugin-owned recovery.
//   * Cancellation: if mountPlugin resolves after the component has
//     already unmounted (Strict Mode double-effect or fast tab toggle),
//     we immediately unmount the leaked handle instead of stashing it.

export interface PluginCapabilityValue {
  pluginId: string;
  mountId: string;
  capability: PluginCapability;
  generation: number;
}

export interface PluginCapabilityError {
  kind: string;
  message: string;
}

export type PluginCapabilityState =
  | { status: "loading" }
  | { status: "ready"; value: PluginCapabilityValue }
  | { status: "error"; error: PluginCapabilityError };

const PluginCapabilityContext = createContext<PluginCapabilityValue | null>(null);

// Plugin components call this to read the capability from the surrounding
// PluginRoot. Throws if mis-used outside the provider, surfacing the bug
// to the error boundary instead of silently passing undefined to invoke.
export function usePluginCapabilityValue(): PluginCapabilityValue {
  const ctx = useContext(PluginCapabilityContext);
  if (!ctx) {
    throw new Error(
      "usePluginCapabilityValue() must be called inside a <PluginRoot> subtree",
    );
  }
  return ctx;
}

// Low-level hook owning the mount lifecycle. Returns the discriminated
// loading/ready/error state matching docs/specs/plugin-contract.md's
// canonical hook flow.
export function usePluginCapability(
  pluginId: string,
  tabId?: string,
): PluginCapabilityState {
  const [state, setState] = useState<PluginCapabilityState>({ status: "loading" });
  const generationRef = useRef(0);

  useEffect(() => {
    let cancelled = false;
    const myGeneration = ++generationRef.current;
    let mintedHandle: PluginCapability | null = null;

    setState({ status: "loading" });

    mountPlugin(pluginId, tabId).then(
      ({ handle, mountId }) => {
        if (cancelled) {
          // The component unmounted (or props changed) before mountPlugin
          // resolved. Don't leak the handle; the Rust registry would keep
          // the entry forever otherwise.
          void unmountPlugin(handle, tabId).catch(() => {});
          return;
        }
        mintedHandle = handle;
        setState({
          status: "ready",
          value: {
            pluginId,
            mountId,
            capability: handle,
            generation: myGeneration,
          },
        });
      },
      (err: unknown) => {
        if (cancelled) return;
        setState({ status: "error", error: toCapabilityError(err) });
      },
    );

    return () => {
      cancelled = true;
      if (mintedHandle) {
        void unmountPlugin(mintedHandle, tabId).catch(() => {});
      }
    };
  }, [pluginId, tabId]);

  return state;
}

function toCapabilityError(err: unknown): PluginCapabilityError {
  // Tauri command errors cross as throws whose payload is the typed DTO.
  // We accept either a structured `{ kind, message }` shape (from the Rust
  // DispatchErrorDto) or a string / Error fallback.
  if (typeof err === "object" && err !== null) {
    const e = err as { kind?: unknown; message?: unknown };
    if (typeof e.kind === "string" && typeof e.message === "string") {
      return { kind: e.kind, message: e.message };
    }
  }
  if (err instanceof Error) {
    return { kind: "unknown", message: err.message };
  }
  if (typeof err === "string") {
    return { kind: "unknown", message: err };
  }
  return { kind: "unknown", message: JSON.stringify(err) };
}

// The host wrapper component plugin tabs render inside. Owns the mount
// lifecycle, surfaces the three required UI states (loading / error with
// retry / ready), and installs an error boundary around the plugin's
// children so render errors are caught and rendered as plugin-owned
// error state instead of crashing the whole tab strip.
export function PluginRoot({
  pluginId,
  label,
  tabId,
  children,
}: {
  pluginId: string;
  label: string;
  tabId?: string;
  children: ReactNode;
}) {
  const state = usePluginCapability(pluginId, tabId);
  const [retryNonce, setRetryNonce] = useState(0);

  // Retry handler bumps a nonce included in the key prop below; React
  // unmounts the inner subtree (triggering unmount of the failed
  // capability if any) and re-runs the effect.
  const handleRetry = useCallback(() => setRetryNonce((n) => n + 1), []);

  if (state.status === "loading") {
    return <LoadingPanel label={label} />;
  }
  if (state.status === "error") {
    return (
      <MountErrorPanel label={label} error={state.error} onRetry={handleRetry} />
    );
  }
  return (
    <PluginCapabilityContext.Provider value={state.value}>
      <PluginErrorBoundary
        pluginId={state.value.pluginId}
        mountId={state.value.mountId}
        label={label}
        onRetry={handleRetry}
        key={`${state.value.mountId}-${retryNonce}`}
      >
        {children}
      </PluginErrorBoundary>
    </PluginCapabilityContext.Provider>
  );
}

function LoadingPanel({ label }: { label: string }) {
  return (
    <section className="placeholder placeholder--loading">
      <h2>{label}</h2>
      <p>正在为插件挂载会话…</p>
    </section>
  );
}

function MountErrorPanel({
  label,
  error,
  onRetry,
}: {
  label: string;
  error: PluginCapabilityError;
  onRetry: () => void;
}) {
  return (
    <section className="placeholder placeholder--error" role="alert">
      <h2>{label} · 挂载失败</h2>
      <dl>
        <dt>错误类型</dt>
        <dd>
          <code>{error.kind}</code>
        </dd>
        <dt>消息</dt>
        <dd>{error.message}</dd>
      </dl>
      <p>插件未能完成挂载，组件已被阻止渲染。可尝试重新挂载；若仍失败请查看 host 日志。</p>
      <button type="button" onClick={onRetry}>
        重试挂载
      </button>
    </section>
  );
}

interface ErrorBoundaryProps {
  pluginId: string;
  mountId: string;
  label: string;
  onRetry: () => void;
  children: ReactNode;
}

interface ErrorBoundaryState {
  error: Error | null;
}

export class PluginErrorBoundary extends Component<
  ErrorBoundaryProps,
  ErrorBoundaryState
> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // NEVER log the capability handle. Identity is `(plugin_id, mount_id)`;
    // the nonce is left to Rust-side logs which already redact sensitive
    // fields.
    console.error(
      `[plugin-error-boundary] plugin=${this.props.pluginId} mount=${this.props.mountId}`,
      error,
      info.componentStack,
    );
  }

  render() {
    if (this.state.error) {
      return (
        <section className="placeholder placeholder--error" role="alert">
          <h2>{this.props.label} · 运行时错误</h2>
          <dl>
            <dt>错误</dt>
            <dd>{this.state.error.message}</dd>
          </dl>
          <p>插件组件在渲染过程中抛出错误。可尝试重新挂载以重置会话。</p>
          <button type="button" onClick={this.props.onRetry}>
            重新挂载插件
          </button>
        </section>
      );
    }
    return this.props.children;
  }
}
