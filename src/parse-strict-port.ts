// Pure helper: parse a user-typed port string with strict
// validation. Unlike `Number.parseInt("22abc", 10)` (which
// returns `22`) or `Number.parseInt("1.5", 10)` (which returns
// `1`), this helper rejects any input that isn't entirely
// decimal digits after trimming. Used by the remote SSH create
// form so a user typo can't silently probe / register a
// different endpoint than the typed string.
//
// Returns `{ ok: true, port }` only when the trimmed input
// matches `/^[0-9]+$/` AND parses to a value in `[1, 65535]`.
// Returns `{ ok: false }` for empty / whitespace-only / partial
// / out-of-range input. The caller decides how to surface the
// rejection (e.g. a `remoteFieldInvalid:port` DTO).

export type ParseStrictPortResult =
  | { ok: true; port: number }
  | { ok: false };

const DECIMAL_DIGITS_ONLY = /^[0-9]+$/;
const MIN_PORT = 1;
const MAX_PORT = 65535;

export function parseStrictPort(raw: string): ParseStrictPortResult {
  const trimmed = raw.trim();
  if (trimmed === "") return { ok: false };
  if (!DECIMAL_DIGITS_ONLY.test(trimmed)) return { ok: false };
  // After the regex check we know all characters are digits,
  // so parseInt cannot lose precision through scientific
  // notation or hexadecimal prefixes. Range-check the result.
  const parsed = Number.parseInt(trimmed, 10);
  if (!Number.isFinite(parsed)) return { ok: false };
  if (parsed < MIN_PORT || parsed > MAX_PORT) return { ok: false };
  return { ok: true, port: parsed };
}
