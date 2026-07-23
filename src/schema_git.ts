export function initializeGitSchema(sql: SqlStorage) {
  sql.exec(`
    CREATE TABLE IF NOT EXISTS objects (
      kind INTEGER NOT NULL,
      id BLOB NOT NULL,
      bytes BLOB NOT NULL,
      PRIMARY KEY (kind, id)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS object_references (
      object_kind INTEGER NOT NULL,
      object_id BLOB NOT NULL,
      reference_kind INTEGER NOT NULL,
      referenced_id BLOB NOT NULL,
      PRIMARY KEY (object_kind, object_id, reference_kind, referenced_id)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pack_uploads (
      pack_id BLOB PRIMARY KEY,
      manifest_length INTEGER NOT NULL,
      pack_length INTEGER NOT NULL,
      pack_hash BLOB NOT NULL,
      chunk_bytes INTEGER NOT NULL,
      object_count INTEGER NOT NULL,
      chunk_count INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pack_upload_manifest_parts (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      bytes BLOB NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pack_upload_heads (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      id BLOB NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pack_upload_objects (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      kind INTEGER NOT NULL,
      id BLOB NOT NULL,
      byte_offset INTEGER NOT NULL,
      byte_length INTEGER NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pack_upload_chunks (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      byte_offset INTEGER NOT NULL,
      byte_length INTEGER NOT NULL,
      hash BLOB NOT NULL,
      received INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pack_upload_chunk_parts (
      pack_id BLOB NOT NULL,
      chunk_position INTEGER NOT NULL,
      part_position INTEGER NOT NULL,
      bytes BLOB NOT NULL,
      PRIMARY KEY (pack_id, chunk_position, part_position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS installed_packs (
      pack_id BLOB PRIMARY KEY,
      manifest_length INTEGER NOT NULL,
      pack_length INTEGER NOT NULL,
      pack_hash BLOB NOT NULL,
      chunk_bytes INTEGER NOT NULL,
      object_count INTEGER NOT NULL,
      chunk_count INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS installed_pack_catalog (
      pack_id BLOB PRIMARY KEY,
      sequence INTEGER NOT NULL UNIQUE
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS installed_pack_manifest_parts (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      bytes BLOB NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS installed_pack_heads (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      id BLOB NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS installed_pack_objects (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      kind INTEGER NOT NULL,
      id BLOB NOT NULL,
      byte_offset INTEGER NOT NULL,
      byte_length INTEGER NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS installed_pack_chunks (
      pack_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      byte_offset INTEGER NOT NULL,
      byte_length INTEGER NOT NULL,
      hash BLOB NOT NULL,
      PRIMARY KEY (pack_id, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS repository_state (
      singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
      incarnation BLOB NOT NULL,
      user_id TEXT NOT NULL,
      repository_id TEXT NOT NULL,
      retired INTEGER NOT NULL DEFAULT 0 CHECK (retired IN (0, 1))
    );
    CREATE TABLE IF NOT EXISTS projection_git_meta (
      singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
      next_fence INTEGER NOT NULL CHECK (next_fence >= 0),
      activation_cursor INTEGER NOT NULL CHECK (activation_cursor >= 0)
    );
    INSERT OR IGNORE INTO projection_git_meta VALUES (1, 0, 0);
    CREATE TABLE IF NOT EXISTS projection_git_receipts (
      canonical_oid BLOB PRIMARY KEY,
      public_oid BLOB NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_git_batches (
      batch_id BLOB PRIMARY KEY,
      remote TEXT NOT NULL,
      owner_machine BLOB NOT NULL,
      fence INTEGER NOT NULL,
      request_hash BLOB NOT NULL,
      created_at_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_git_states (
      state_id INTEGER PRIMARY KEY AUTOINCREMENT,
      remote TEXT NOT NULL,
      bookmark TEXT NOT NULL,
      canonical_oid BLOB NOT NULL,
      public_oid BLOB NOT NULL,
      hidden_set_id BLOB,
      pending_batch_id BLOB,
      activation_seq INTEGER UNIQUE
    );
    CREATE INDEX IF NOT EXISTS projection_git_states_remote_bookmark_activation
      ON projection_git_states (remote, bookmark, activation_seq);
    CREATE INDEX IF NOT EXISTS projection_git_states_pending_batch
      ON projection_git_states (pending_batch_id)
      WHERE pending_batch_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS projection_git_states_canonical_oid
      ON projection_git_states (canonical_oid);
    CREATE TABLE IF NOT EXISTS projection_git_batch_refs (
      batch_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      remote TEXT NOT NULL,
      bookmark TEXT NOT NULL,
      expected_old_oid BLOB,
      proposed_state_id INTEGER,
      PRIMARY KEY (batch_id, position),
      UNIQUE (remote, bookmark)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_git_cursors (
      remote TEXT NOT NULL,
      bookmark TEXT NOT NULL,
      state_id INTEGER NOT NULL,
      PRIMARY KEY (remote, bookmark)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_git_batch_results (
      batch_id BLOB PRIMARY KEY,
      remote TEXT NOT NULL,
      request_hash BLOB NOT NULL,
      final_fence INTEGER NOT NULL,
      outcome TEXT NOT NULL CHECK (outcome IN ('accepted', 'aborted')),
      finished_at_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS projection_git_batch_results_finished_at
      ON projection_git_batch_results (finished_at_ms);
    CREATE TABLE IF NOT EXISTS projection_git_recovery_claims (
      batch_id BLOB PRIMARY KEY
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_git_fetch_results (
      fetch_id BLOB PRIMARY KEY,
      remote TEXT NOT NULL,
      request_hash BLOB NOT NULL,
      activation_cursor INTEGER NOT NULL CHECK (activation_cursor >= 0),
      created_at_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS projection_git_fetch_results_created_at
      ON projection_git_fetch_results (created_at_ms);
    CREATE TABLE IF NOT EXISTS projection_git_remotes (
      name TEXT PRIMARY KEY,
      url TEXT NOT NULL
    ) WITHOUT ROWID;
  `);
}
