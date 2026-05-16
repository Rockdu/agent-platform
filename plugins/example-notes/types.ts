// Typed contracts for example-notes plugin commands.
//
// Referenced by name from plugins/example-notes/plugin.toml [[commands]]
// (`args_type` / `result_type` fields). plugin-codegen generates a per-plugin
// wrapper that imports these and produces typed `commands.<name>(args, capability)`.

export interface ListNotesArgs {
  limit?: number;
}

export interface ListNotesResult {
  notes: Array<{ id: string; body: string }>;
}

export interface CreateNoteArgs {
  body: string;
}

export interface CreateNoteResult {
  id: string;
}
