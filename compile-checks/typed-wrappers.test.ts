// Compile-time probe: proves the generated per-plugin command wrappers carry
// concrete TS types (not `unknown`). This file is included by tsconfig.json so
// `tsc -b` fails if:
//   a) the generated wrapper is no longer typed (the `@ts-expect-error` below
//      becomes a "unused" diagnostic), OR
//   b) the imported types disappear from plugins/example-notes/types.ts.
//
// Run automatically by `npm run build` and `npm run dev` (via tsc -b).
// Not a runtime test; bodies should not execute.

import { commands, type PluginCapability } from "../src/generated/plugins/example-notes";
import type {
  CreateNoteArgs,
  CreateNoteResult,
  ListNotesArgs,
  ListNotesResult,
} from "../plugins/example-notes/types";

// Fake capability handle; types-only.
declare const capability: PluginCapability;

export async function _probe_valid_calls(): Promise<void> {
  const createArgs: CreateNoteArgs = { body: "valid" };
  const createResult: CreateNoteResult = await commands.create_note(createArgs, capability);
  void createResult.id;

  const listArgs: ListNotesArgs = { limit: 10 };
  const listResult: ListNotesResult = await commands.list_notes(listArgs, capability);
  void listResult.notes;
}

export async function _probe_invalid_calls(): Promise<void> {
  // Wrong arg shape — `body` must be a string, not a number.
  // @ts-expect-error wrong arg shape proves the wrappers are typed
  await commands.create_note({ body: 42 }, capability);

  // Missing required field — `CreateNoteArgs.body` is required.
  // @ts-expect-error wrong arg shape proves the wrappers are typed
  await commands.create_note({}, capability);

  // Wrong field name.
  // @ts-expect-error wrong arg shape proves the wrappers are typed
  await commands.create_note({ wrong: "value" }, capability);
}
