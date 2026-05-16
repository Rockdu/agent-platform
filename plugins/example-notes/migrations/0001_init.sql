-- Initial schema for the example-notes plugin. Lexically first file in
-- `migrations/` so the per-plugin migration runner applies it first.

CREATE TABLE notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_notes_created_at ON notes(created_at);
