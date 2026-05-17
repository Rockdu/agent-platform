import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SidecarErrorDto, SidecarStatusSnapshot } from "./sidecar-status";

export interface UseSidecarStatusResult {
  status: SidecarStatusSnapshot | null;
  error: SidecarErrorDto | null;
  retry: () => Promise<void>;
  shutdown: () => Promise<void>;
}

export interface UseSidecarStatusOptions {
  pollIntervalMs?: number;
  enabled?: boolean;
}

const DEFAULT_POLL_INTERVAL_MS = 500;

export function useSidecarStatus(
  clientId: string | null,
  options: UseSidecarStatusOptions = {},
): UseSidecarStatusResult {
  const pollIntervalMs = options.pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS;
  const enabled = options.enabled ?? true;
  const [status, setStatus] = useState<SidecarStatusSnapshot | null>(null);
  const [error, setError] = useState<SidecarErrorDto | null>(null);
  const cancelledRef = useRef(false);

  const fetchOnce = useCallback(async () => {
    if (!clientId) return;
    try {
      const snap = await invoke<SidecarStatusSnapshot | null>(
        "sidecar_status",
        { clientId },
      );
      if (cancelledRef.current) return;
      setStatus(snap);
      setError(null);
    } catch (err) {
      if (cancelledRef.current) return;
      setError(coerceErrorDto(err));
    }
  }, [clientId]);

  useEffect(() => {
    cancelledRef.current = false;
    if (!enabled || !clientId) {
      return () => {
        cancelledRef.current = true;
      };
    }
    void fetchOnce();
    const handle = window.setInterval(fetchOnce, pollIntervalMs);
    return () => {
      cancelledRef.current = true;
      window.clearInterval(handle);
    };
  }, [enabled, clientId, fetchOnce, pollIntervalMs]);

  const retry = useCallback(async () => {
    if (!clientId) return;
    try {
      await invoke<void>("retry_sidecar", { clientId });
      setError(null);
      await fetchOnce();
    } catch (err) {
      setError(coerceErrorDto(err));
    }
  }, [clientId, fetchOnce]);

  const shutdown = useCallback(async () => {
    if (!clientId) return;
    try {
      await invoke<void>("shutdown_sidecar", { clientId });
      setError(null);
      await fetchOnce();
    } catch (err) {
      setError(coerceErrorDto(err));
    }
  }, [clientId, fetchOnce]);

  return { status, error, retry, shutdown };
}

export async function spawnSidecarFromManifest(
  clientId: string,
): Promise<SidecarErrorDto | null> {
  try {
    await invoke<void>("spawn_sidecar_from_manifest", { clientId });
    return null;
  } catch (err) {
    const dto = coerceErrorDto(err);
    if (dto.kind === "alreadyMounted") return null;
    return dto;
  }
}

function coerceErrorDto(err: unknown): SidecarErrorDto {
  if (isSidecarErrorDto(err)) return err;
  return {
    kind: "io",
    context: "tauri-invoke",
    message: typeof err === "string" ? err : JSON.stringify(err),
  };
}

function isSidecarErrorDto(value: unknown): value is SidecarErrorDto {
  if (typeof value !== "object" || value === null) return false;
  const kind = (value as { kind?: unknown }).kind;
  return (
    kind === "alreadyMounted" ||
    kind === "notFound" ||
    kind === "invalidState" ||
    kind === "invalidClientId" ||
    kind === "unknownPlugin" ||
    kind === "missingBinary" ||
    kind === "io" ||
    kind === "encodeShutdown" ||
    kind === "signal"
  );
}
