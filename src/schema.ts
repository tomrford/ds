export function initializeSchema(sql: SqlStorage) {
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
      referenced_kind INTEGER NOT NULL,
      referenced_id BLOB NOT NULL,
      PRIMARY KEY (object_kind, object_id, referenced_kind, referenced_id)
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
  `);

  sql.exec(`
    CREATE TABLE IF NOT EXISTS repository_state (
      singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
      incarnation BLOB NOT NULL,
      user_id TEXT,
      repository_id TEXT,
      retired INTEGER NOT NULL DEFAULT 0 CHECK (retired IN (0, 1)),
      cursor INTEGER NOT NULL CHECK (cursor >= 0),
      receipt_count INTEGER NOT NULL CHECK (receipt_count >= 0),
      receipt_head_count INTEGER NOT NULL CHECK (receipt_head_count >= 0)
    );
    CREATE TABLE IF NOT EXISTS operation_heads (
      id BLOB PRIMARY KEY
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS head_transactions (
      incarnation BLOB NOT NULL,
      idempotency_key BLOB NOT NULL,
      request_hash BLOB NOT NULL,
      cursor INTEGER NOT NULL,
      created_at_ms INTEGER NOT NULL,
      PRIMARY KEY (incarnation, idempotency_key)
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS head_transactions_created_at
      ON head_transactions (created_at_ms);
    CREATE TABLE IF NOT EXISTS head_transaction_heads (
      incarnation BLOB NOT NULL,
      idempotency_key BLOB NOT NULL,
      position INTEGER NOT NULL,
      id BLOB NOT NULL,
      PRIMARY KEY (incarnation, idempotency_key, position)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS pending_observed_heads (
      id BLOB PRIMARY KEY
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS complete_object_closures (
      kind INTEGER NOT NULL,
      id BLOB NOT NULL,
      PRIMARY KEY (kind, id)
    ) WITHOUT ROWID;
  `);

  migrateRepositoryAuthority(sql);

  sql.exec(`
    CREATE TABLE IF NOT EXISTS projection_meta (
      singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
      next_fence INTEGER NOT NULL CHECK (next_fence >= 0),
      activation_cursor INTEGER NOT NULL CHECK (activation_cursor >= 0)
    );
    INSERT OR IGNORE INTO projection_meta VALUES (1, 0, 0);
    CREATE TABLE IF NOT EXISTS git_receipts (
      git_oid BLOB PRIMARY KEY,
      public_commit_id BLOB NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_batches (
      batch_id BLOB PRIMARY KEY,
      remote TEXT NOT NULL,
      owner_machine BLOB NOT NULL,
      fence INTEGER NOT NULL,
      request_hash BLOB NOT NULL,
      created_at_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_states (
      state_id INTEGER PRIMARY KEY AUTOINCREMENT,
      remote TEXT NOT NULL,
      bookmark TEXT NOT NULL,
      git_oid BLOB NOT NULL,
      canonical_commit_id BLOB NOT NULL,
      public_commit_id BLOB NOT NULL,
      hidden_set_id BLOB,
      pending_batch_id BLOB,
      activation_seq INTEGER UNIQUE
    );
    CREATE TABLE IF NOT EXISTS projection_batch_refs (
      batch_id BLOB NOT NULL,
      position INTEGER NOT NULL,
      remote TEXT NOT NULL,
      bookmark TEXT NOT NULL,
      expected_old_oid BLOB,
      proposed_state_id INTEGER,
      PRIMARY KEY (batch_id, position),
      UNIQUE (remote, bookmark)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_cursors (
      remote TEXT NOT NULL,
      bookmark TEXT NOT NULL,
      state_id INTEGER NOT NULL,
      PRIMARY KEY (remote, bookmark)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_batch_results (
      batch_id BLOB PRIMARY KEY,
      remote TEXT NOT NULL,
      request_hash BLOB NOT NULL,
      final_fence INTEGER NOT NULL,
      outcome TEXT NOT NULL CHECK (outcome IN ('accepted', 'aborted')),
      finished_at_ms INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS projection_recovery_claims (
      batch_id BLOB PRIMARY KEY
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS remotes (
      name TEXT PRIMARY KEY,
      url TEXT NOT NULL
    ) WITHOUT ROWID;
  `);
}

function migrateRepositoryAuthority(sql: SqlStorage) {
  sql.exec(`
    CREATE TABLE IF NOT EXISTS _sql_schema_migrations (
      id INTEGER PRIMARY KEY,
      applied_at_ms INTEGER NOT NULL
    );
  `);
  const columns = new Set(
    sql
      .exec<{ name: string }>("PRAGMA table_info(repository_state)")
      .toArray()
      .map((column) => column.name),
  );
  if (!columns.has("user_id")) sql.exec("ALTER TABLE repository_state ADD COLUMN user_id TEXT");
  if (!columns.has("repository_id")) {
    sql.exec("ALTER TABLE repository_state ADD COLUMN repository_id TEXT");
  }
  if (!columns.has("retired")) {
    sql.exec("ALTER TABLE repository_state ADD COLUMN retired INTEGER NOT NULL DEFAULT 0");
  }
  sql.exec("INSERT OR IGNORE INTO _sql_schema_migrations VALUES (2, ?)", Date.now());
}
