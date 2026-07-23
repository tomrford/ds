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
  `);
}
