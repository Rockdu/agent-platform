// Typed contracts for the papers plugin commands.
//
// Referenced by name from plugins/papers/plugin.toml [[commands]]
// (`args_type` / `result_type` fields). plugin-codegen generates
// a per-plugin wrapper that imports these and produces typed
// `commands.<name>(args, capability)` calls.

export interface PaperRecord {
  arxivId: string;
  title: string;
  authors: string[];
  abstractSnippet: string;
  pdfUrl: string;
  absUrl: string;
  fetchedAt: string;
  source: "scheduled" | "manual";
  /** Hydrated from user_paper_state via LEFT JOIN. */
  starred: boolean;
  /** Hydrated from user_paper_state via LEFT JOIN. Null if unread. */
  readAt: string | null;
}

export interface ListRecentArgs {
  limit?: number;
}

export interface ListRecentResult {
  papers: PaperRecord[];
}

export interface SearchArgs {
  query: string;
}

export interface SearchResult {
  papers: PaperRecord[];
}
