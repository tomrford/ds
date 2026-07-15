import { DurableObject } from "cloudflare:workers";
import { HeadStore } from "./head_store";
import { MAX_HEAD_REQUEST_BYTES } from "./head_protocol";
import { Kernel, equalBytes, exactBuffer, fromHex, toHex } from "./kernel";
import {
  MAX_CHUNK_BYTES,
  MAX_MANIFEST_BYTES,
  MAX_OBJECT_BYTES,
  PackManifest,
  concatParts,
  decodeManifest,
  readBoundedBody,
  splitParts,
} from "./pack_protocol";
import { MAX_PROJECTION_REQUEST_BYTES } from "./projection_protocol";
import { ProjectionStore } from "./projection_store";

const REPOSITORY_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const PACK_ID_PATTERN = /^[0-9a-f]{128}$/;
const PACK_CATALOG_PAGE = 256;

interface Env {
  REPOSITORIES: DurableObjectNamespace<Repository>;
  SPIKE_TOKEN: string;
}

interface UploadRow extends Record<string, SqlStorageValue> {
  manifest_length: number;
  pack_length: number;
  pack_hash: ArrayBuffer;
  chunk_bytes: number;
  object_count: number;
  chunk_count: number;
}

interface InstalledPackRow extends Record<string, SqlStorageValue> {
  pack_id: ArrayBuffer;
  sequence: number;
}

interface ObjectRow extends Record<string, SqlStorageValue> {
  position: number;
  kind: number;
  id: ArrayBuffer;
  byte_offset: number;
  byte_length: number;
}

interface ChunkRow extends Record<string, SqlStorageValue> {
  position: number;
  byte_offset: number;
  byte_length: number;
  hash: ArrayBuffer;
  received: number;
}

interface BytesRow extends Record<string, SqlStorageValue> {
  bytes: ArrayBuffer;
}

interface InstalledObjectBytesRow extends BytesRow {
  byte_offset: number;
  byte_length: number;
}

class PackValidationError extends Error {}

export class Repository extends DurableObject<Env> {
  private kernel = new Kernel();
  private heads: HeadStore;
  private projection: ProjectionStore;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.ctx.storage.sql.exec(`
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
    this.heads = new HeadStore(this.ctx, this.kernel);
    this.projection = new ProjectionStore(this.ctx, this.kernel);
  }

  putPackManifest(packId: string, bytes: Uint8Array) {
    try {
      const id = exactBuffer(fromHex(packId));
      const actualId = this.kernel.hash([bytes]);
      if (!equalBytes(actualId, new Uint8Array(id))) {
        throw new PackValidationError(`manifest hashes to ${toHex(actualId)}, not ${packId}`);
      }
      let manifest: PackManifest;
      try {
        manifest = decodeManifest(bytes);
      } catch (error) {
        throw new PackValidationError(
          error instanceof Error ? error.message : "manifest validation failed",
        );
      }
      const installed = this.installed(id);
      if (installed !== undefined) {
        const installedBytes = concatParts(
          this.installedManifestParts(id),
          Math.min(installed.manifest_length, MAX_MANIFEST_BYTES),
        );
        if (!equalBytes(installedBytes, bytes)) {
          throw new Error("installed pack ID collision with different manifest bytes");
        }
        return { ok: true as const, inserted: false, installed: true };
      }
      const existing = this.upload(id);
      if (existing !== undefined) {
        const existingBytes = concatParts(
          this.manifestParts(id),
          Math.min(existing.manifest_length, MAX_MANIFEST_BYTES),
        );
        if (!equalBytes(existingBytes, bytes)) {
          throw new Error("pack ID collision with different manifest bytes");
        }
        return { ok: true as const, inserted: false, installed: false };
      }
      this.storeManifest(id, bytes, manifest);
      return { ok: true as const, inserted: true, installed: false };
    } catch (error) {
      this.resetTrappedKernel(error);
      if (error instanceof PackValidationError) {
        return {
          ok: false as const,
          error: error instanceof Error ? error.message : "manifest validation failed",
        };
      }
      throw error;
    }
  }

  putPackChunk(packId: string, position: number, bytes: Uint8Array) {
    try {
      const id = exactBuffer(fromHex(packId));
      const installed = this.installed(id);
      const expected =
        installed === undefined
          ? this.ctx.storage.sql
              .exec<ChunkRow>(
                `SELECT position, byte_offset, byte_length, hash, received
                 FROM pack_upload_chunks WHERE pack_id = ? AND position = ?`,
                id,
                position,
              )
              .toArray()[0]
          : this.ctx.storage.sql
              .exec<ChunkRow>(
                `SELECT position, byte_offset, byte_length, hash, 1 AS received
                 FROM installed_pack_chunks WHERE pack_id = ? AND position = ?`,
                id,
                position,
              )
              .toArray()[0];
      if (expected === undefined) throw new PackValidationError("unknown pack or chunk position");
      if (bytes.byteLength !== expected.byte_length) {
        throw new PackValidationError(
          `chunk ${position} is ${bytes.byteLength} bytes, expected ${expected.byte_length}`,
        );
      }
      const actualHash = this.kernel.hash([bytes]);
      if (!equalBytes(actualHash, new Uint8Array(expected.hash))) {
        throw new PackValidationError(`chunk ${position} hash does not match manifest`);
      }
      if (installed !== undefined) {
        return { ok: true as const, inserted: false, installed: true };
      }
      if (expected.received !== 0) {
        const existing = concatParts(this.chunkParts(id, position), expected.byte_length);
        if (!equalBytes(existing, bytes)) throw new Error("chunk hash collision with different bytes");
        return { ok: true as const, inserted: false, installed: false };
      }
      this.ctx.storage.transactionSync(() => {
        for (const [partPosition, part] of splitParts(bytes).entries()) {
          this.ctx.storage.sql.exec(
            "INSERT INTO pack_upload_chunk_parts VALUES (?, ?, ?, ?)",
            id,
            position,
            partPosition,
            exactBuffer(part),
          );
        }
        this.ctx.storage.sql.exec(
          "UPDATE pack_upload_chunks SET received = 1 WHERE pack_id = ? AND position = ?",
          id,
          position,
        );
      });
      return { ok: true as const, inserted: true, installed: false };
    } catch (error) {
      this.resetTrappedKernel(error);
      if (error instanceof PackValidationError) {
        return {
          ok: false as const,
          error: error instanceof Error ? error.message : "chunk validation failed",
        };
      }
      throw error;
    }
  }

  installPack(packId: string) {
    let id: ArrayBuffer;
    try {
      id = exactBuffer(fromHex(packId));
    } catch (error) {
      return { ok: false as const, error: error instanceof Error ? error.message : "invalid pack ID" };
    }
    if (this.installed(id) !== undefined) {
      return { ok: true as const, installed: false, insertedObjects: 0 };
    }
    const upload = this.upload(id);
    if (upload === undefined) return { ok: false as const, error: "pack manifest is not uploaded" };
    const missing = this.ctx.storage.sql
      .exec<{ count: number }>(
        "SELECT count(*) AS count FROM pack_upload_chunks WHERE pack_id = ? AND received = 0",
        id,
      )
      .one().count;
    if (missing !== 0) return { ok: false as const, error: `pack is missing ${missing} chunks` };

    try {
      const insertedObjects = this.ctx.storage.transactionSync(() => this.installUploadedPack(id, upload));
      return { ok: true as const, installed: true, insertedObjects };
    } catch (error) {
      this.resetTrappedKernel(error);
      if (error instanceof PackValidationError) {
        return { ok: false as const, error: error.message };
      }
      throw error;
    }
  }

  countObjects(): number {
    return this.ctx.storage.sql.exec<{ count: number }>("SELECT count(*) AS count FROM objects").one()
      .count;
  }

  countInstalledPacks(): number {
    return this.ctx.storage.sql
      .exec<{ count: number }>("SELECT count(*) AS count FROM installed_packs")
      .one().count;
  }

  initializeRepository(incarnationValue: unknown) {
    return this.heads.initialize(incarnationValue);
  }

  getHeads(incarnationValue: unknown) {
    return this.heads.get(incarnationValue);
  }

  transactHeads(value: unknown) {
    return this.heads.transact(value);
  }

  getProjection(incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    return this.projection.get(incarnationValue, afterValue, throughValue);
  }

  mutateHiddenPolicy(value: unknown) {
    return this.projection.mutatePolicy(value);
  }

  beginProjectionPush(value: unknown) {
    return this.projection.begin(value);
  }

  claimProjectionPush(batchId: unknown, value: unknown) {
    return this.projection.claim(batchId, value);
  }

  getProjectionPushReplay(batchId: unknown, incarnationValue: unknown) {
    return this.projection.replay(batchId, incarnationValue);
  }

  confirmProjectionPush(batchId: unknown, value: unknown) {
    return this.projection.confirm(batchId, value);
  }

  recoverProjectionPush(batchId: unknown, value: unknown) {
    return this.projection.recover(batchId, value);
  }

  listInstalledPacks(incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    const state = this.heads.get(incarnationValue);
    if (!state.ok) return state;
    if (typeof afterValue !== "number" || !Number.isSafeInteger(afterValue) || afterValue < 0) {
      return { ok: false as const, status: 400, error: "pack cursor must be a non-negative integer" };
    }
    const highWater = this.installedPackHighWater();
    const through = throughValue === undefined ? highWater : throughValue;
    if (
      typeof through !== "number" ||
      !Number.isSafeInteger(through) ||
      through < afterValue ||
      through > highWater
    ) {
      return {
        ok: false as const,
        status: 400,
        error: "pack high-water must be between the cursor and current catalog frontier",
      };
    }
    const rows = this.ctx.storage.sql
      .exec<InstalledPackRow>(
        `SELECT pack_id, sequence FROM installed_pack_catalog
         WHERE sequence > ? AND sequence <= ? ORDER BY sequence LIMIT ?`,
        afterValue,
        through,
        PACK_CATALOG_PAGE + 1,
      )
      .toArray();
    const hasMore = rows.length > PACK_CATALOG_PAGE;
    const page = rows.slice(0, PACK_CATALOG_PAGE);
    return {
      ok: true as const,
      packs: page.map((row) => ({ sequence: row.sequence, id: toHex(new Uint8Array(row.pack_id)) })),
      nextAfter: page.at(-1)?.sequence ?? afterValue,
      through,
      hasMore,
    };
  }

  getInstalledPackManifest(packId: string, incarnationValue: unknown) {
    const state = this.heads.get(incarnationValue);
    if (!state.ok) return state;
    let id: ArrayBuffer;
    try {
      id = exactBuffer(fromHex(packId));
    } catch (error) {
      return {
        ok: false as const,
        status: 400,
        error: error instanceof Error ? error.message : "invalid pack ID",
      };
    }
    const installed = this.installed(id);
    if (installed === undefined) {
      return { ok: false as const, status: 404, error: "installed pack does not exist" };
    }
    const bytes = concatParts(
      this.installedManifestParts(id),
      Math.min(installed.manifest_length, MAX_MANIFEST_BYTES),
    );
    if (bytes.byteLength !== installed.manifest_length) {
      throw new Error("installed pack manifest is incomplete");
    }
    if (!equalBytes(this.kernel.hash([bytes]), new Uint8Array(id))) {
      throw new Error("installed pack manifest hash changed");
    }
    return { ok: true as const, bytes: exactBuffer(bytes) };
  }

  getInstalledPackChunk(packId: string, position: number, incarnationValue: unknown) {
    const state = this.heads.get(incarnationValue);
    if (!state.ok) return state;
    let id: ArrayBuffer;
    try {
      id = exactBuffer(fromHex(packId));
    } catch (error) {
      return {
        ok: false as const,
        status: 400,
        error: error instanceof Error ? error.message : "invalid pack ID",
      };
    }
    const chunk = this.ctx.storage.sql
      .exec<ChunkRow>(
        `SELECT position, byte_offset, byte_length, hash, 1 AS received
         FROM installed_pack_chunks WHERE pack_id = ? AND position = ?`,
        id,
        position,
      )
      .toArray()[0];
    if (chunk === undefined) {
      return { ok: false as const, status: 404, error: "installed pack chunk does not exist" };
    }
    const end = chunk.byte_offset + chunk.byte_length;
    const objects = this.ctx.storage.sql
      .exec<InstalledObjectBytesRow>(
        `SELECT indexed.byte_offset, indexed.byte_length, objects.bytes
         FROM installed_pack_objects AS indexed
         JOIN objects ON objects.kind = indexed.kind AND objects.id = indexed.id
         WHERE indexed.pack_id = ?
           AND indexed.byte_offset < ?
           AND indexed.byte_offset + indexed.byte_length > ?
         ORDER BY indexed.position`,
        id,
        end,
        chunk.byte_offset,
      )
      .toArray();
    const bytes = new Uint8Array(chunk.byte_length);
    let filled = 0;
    for (const object of objects) {
      const objectBytes = new Uint8Array(object.bytes);
      if (objectBytes.byteLength !== object.byte_length) {
        throw new Error("installed object length does not match its pack index");
      }
      const overlapStart = Math.max(chunk.byte_offset, object.byte_offset);
      const overlapEnd = Math.min(end, object.byte_offset + object.byte_length);
      if (overlapStart !== chunk.byte_offset + filled || overlapEnd <= overlapStart) {
        throw new Error("installed pack object ranges do not reconstruct the requested chunk");
      }
      bytes.set(
        objectBytes.subarray(overlapStart - object.byte_offset, overlapEnd - object.byte_offset),
        filled,
      );
      filled += overlapEnd - overlapStart;
    }
    if (filled !== bytes.byteLength) {
      throw new Error("installed pack chunk is incomplete");
    }
    if (!equalBytes(this.kernel.hash([bytes]), new Uint8Array(chunk.hash))) {
      throw new Error("installed pack chunk hash changed");
    }
    return { ok: true as const, bytes: exactBuffer(bytes) };
  }

  private storeManifest(id: ArrayBuffer, bytes: Uint8Array, manifest: PackManifest) {
    this.ctx.storage.transactionSync(() => {
      this.ctx.storage.sql.exec(
        "INSERT INTO pack_uploads VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        bytes.byteLength,
        manifest.packLength,
        exactBuffer(manifest.packHash),
        manifest.chunkBytes,
        manifest.objects.length,
        manifest.chunks.length,
      );
      for (const [position, part] of splitParts(bytes).entries()) {
        this.ctx.storage.sql.exec(
          "INSERT INTO pack_upload_manifest_parts VALUES (?, ?, ?)",
          id,
          position,
          exactBuffer(part),
        );
      }
      for (const [position, head] of manifest.operationHeads.entries()) {
        this.ctx.storage.sql.exec(
          "INSERT INTO pack_upload_heads VALUES (?, ?, ?)",
          id,
          position,
          exactBuffer(head),
        );
      }
      for (const [position, object] of manifest.objects.entries()) {
        this.ctx.storage.sql.exec(
          "INSERT INTO pack_upload_objects VALUES (?, ?, ?, ?, ?, ?)",
          id,
          position,
          object.kind,
          exactBuffer(object.id),
          object.offset,
          object.length,
        );
      }
      for (const [position, chunk] of manifest.chunks.entries()) {
        this.ctx.storage.sql.exec(
          "INSERT INTO pack_upload_chunks VALUES (?, ?, ?, ?, ?, 0)",
          id,
          position,
          chunk.offset,
          chunk.length,
          exactBuffer(chunk.hash),
        );
      }
    });
  }

  private installUploadedPack(id: ArrayBuffer, upload: UploadRow): number {
    const objects = this.ctx.storage.sql
      .exec<ObjectRow>(
        `SELECT position, kind, id, byte_offset, byte_length
         FROM pack_upload_objects WHERE pack_id = ? ORDER BY position`,
        id,
      )
      .toArray();
    const chunks = this.ctx.storage.sql
      .exec<ChunkRow>(
        `SELECT position, byte_offset, byte_length, hash, received
         FROM pack_upload_chunks WHERE pack_id = ? ORDER BY position`,
        id,
      )
      .toArray();
    if (objects.length !== upload.object_count || chunks.length !== upload.chunk_count) {
      throw new PackValidationError("quarantined pack index is incomplete");
    }

    let objectIndex = 0;
    let objectWritten = 0;
    let objectBytes = objects.length === 0 ? undefined : new Uint8Array(objects[0].byte_length);
    let packOffset = 0;
    let insertedObjects = 0;
    const packHash = this.kernel.startHash();
    try {
      const finishReadyObjects = () => {
        while (objectIndex < objects.length && objectWritten === objects[objectIndex].byte_length) {
          const object = objects[objectIndex];
          if (object.byte_offset + object.byte_length !== packOffset) break;
          insertedObjects += this.validateAndStoreManifestObject(object, objectBytes!);
          objectIndex += 1;
          objectWritten = 0;
          objectBytes =
            objectIndex < objects.length ? new Uint8Array(objects[objectIndex].byte_length) : undefined;
        }
      };
      finishReadyObjects();

      for (const chunk of chunks) {
        const parts = this.chunkParts(id, chunk.position);
        const chunkLength = parts.reduce((length, part) => length + part.byteLength, 0);
        if (chunkLength !== chunk.byte_length) {
          throw new PackValidationError(`stored chunk ${chunk.position} length changed`);
        }
        const chunkHash = this.kernel.hash(parts);
        if (!equalBytes(chunkHash, new Uint8Array(chunk.hash))) {
          throw new PackValidationError(`stored chunk ${chunk.position} hash changed`);
        }
        for (const part of parts) {
          packHash.update(part);
          let partOffset = 0;
          while (partOffset < part.byteLength) {
            if (objectIndex >= objects.length || objectBytes === undefined) {
              throw new PackValidationError("pack contains bytes outside its object ranges");
            }
            const object = objects[objectIndex];
            if (object.byte_offset + objectWritten !== packOffset) {
              throw new PackValidationError("pack object ranges changed after manifest upload");
            }
            const count = Math.min(object.byte_length - objectWritten, part.byteLength - partOffset);
            objectBytes.set(part.subarray(partOffset, partOffset + count), objectWritten);
            objectWritten += count;
            partOffset += count;
            packOffset += count;
            finishReadyObjects();
          }
        }
      }
      finishReadyObjects();
      if (packOffset !== upload.pack_length || objectIndex !== objects.length) {
        throw new PackValidationError("pack bytes do not fill every object range");
      }
      const actualPackHash = packHash.finish();
      if (!equalBytes(actualPackHash, new Uint8Array(upload.pack_hash))) {
        throw new PackValidationError("whole-pack hash does not match manifest");
      }
    } finally {
      packHash.dispose();
    }

    this.ctx.storage.sql.exec(
      "INSERT INTO installed_packs VALUES (?, ?, ?, ?, ?, ?, ?)",
      id,
      upload.manifest_length,
      upload.pack_length,
      upload.pack_hash,
      upload.chunk_bytes,
      upload.object_count,
      upload.chunk_count,
    );
    this.ctx.storage.sql.exec(
      "INSERT INTO installed_pack_catalog VALUES (?, ?)",
      id,
      this.nextInstalledPackSequence(),
    );
    this.ctx.storage.sql.exec(
      `INSERT INTO installed_pack_manifest_parts
       SELECT pack_id, position, bytes FROM pack_upload_manifest_parts WHERE pack_id = ?`,
      id,
    );
    this.ctx.storage.sql.exec(
      "INSERT INTO installed_pack_heads SELECT pack_id, position, id FROM pack_upload_heads WHERE pack_id = ?",
      id,
    );
    this.ctx.storage.sql.exec(
      `INSERT INTO installed_pack_objects
       SELECT pack_id, position, kind, id, byte_offset, byte_length
       FROM pack_upload_objects WHERE pack_id = ?`,
      id,
    );
    this.ctx.storage.sql.exec(
      `INSERT INTO installed_pack_chunks
       SELECT pack_id, position, byte_offset, byte_length, hash
       FROM pack_upload_chunks WHERE pack_id = ?`,
      id,
    );
    for (const table of [
      "pack_upload_chunk_parts",
      "pack_upload_chunks",
      "pack_upload_objects",
      "pack_upload_heads",
      "pack_upload_manifest_parts",
      "pack_uploads",
    ]) {
      this.ctx.storage.sql.exec(`DELETE FROM ${table} WHERE pack_id = ?`, id);
    }
    return insertedObjects;
  }

  private validateAndStoreManifestObject(object: ObjectRow, bytes: Uint8Array): number {
    if (bytes.byteLength > MAX_OBJECT_BYTES) {
      throw new PackValidationError(`object ${object.position} exceeds the object byte limit`);
    }
    let validated;
    try {
      validated = this.kernel.validate(object.kind, bytes);
    } catch (error) {
      if (error instanceof WebAssembly.RuntimeError) throw error;
      throw new PackValidationError(
        `object ${object.position} is invalid: ${error instanceof Error ? error.message : "validation failed"}`,
      );
    }
    if (!equalBytes(validated.id, new Uint8Array(object.id))) {
      throw new PackValidationError(`object ${object.position} ID does not match manifest`);
    }
    return this.storeValidatedObject(object.kind, validated.id, bytes, validated.references) ? 1 : 0;
  }

  private storeValidatedObject(
    kind: number,
    idBytes: Uint8Array,
    bytes: Uint8Array,
    references: Array<{ kind: number; id: Uint8Array }>,
  ): boolean {
    const id = exactBuffer(idBytes);
    const existing = this.ctx.storage.sql
      .exec<{ bytes: ArrayBuffer }>("SELECT bytes FROM objects WHERE kind = ? AND id = ?", kind, id)
      .toArray();
    if (existing.length !== 0) {
      if (!equalBytes(new Uint8Array(existing[0].bytes), bytes)) {
        throw new Error("content ID collision with different canonical bytes");
      }
      return false;
    }
    this.ctx.storage.sql.exec(
      "INSERT INTO objects (kind, id, bytes) VALUES (?, ?, ?)",
      kind,
      id,
      exactBuffer(bytes),
    );
    for (const reference of references) {
      this.ctx.storage.sql.exec(
        "INSERT INTO object_references VALUES (?, ?, ?, ?)",
        kind,
        id,
        reference.kind,
        exactBuffer(reference.id),
      );
    }
    return true;
  }

  private upload(id: ArrayBuffer): UploadRow | undefined {
    return this.ctx.storage.sql
      .exec<UploadRow>(
        `SELECT manifest_length, pack_length, pack_hash, chunk_bytes, object_count, chunk_count
         FROM pack_uploads WHERE pack_id = ?`,
        id,
      )
      .toArray()[0];
  }

  private installed(id: ArrayBuffer): UploadRow | undefined {
    return this.ctx.storage.sql
      .exec<UploadRow>(
        `SELECT manifest_length, pack_length, pack_hash, chunk_bytes, object_count, chunk_count
         FROM installed_packs WHERE pack_id = ?`,
        id,
      )
      .toArray()[0];
  }

  private nextInstalledPackSequence(): number {
    const previous = this.installedPackHighWater();
    if (previous >= Number.MAX_SAFE_INTEGER) throw new Error("installed pack sequence exhausted");
    return previous + 1;
  }

  private installedPackHighWater(): number {
    return this.ctx.storage.sql
      .exec<{ sequence: number }>(
        "SELECT COALESCE(MAX(sequence), 0) AS sequence FROM installed_pack_catalog",
      )
      .one().sequence;
  }

  private manifestParts(id: ArrayBuffer): Uint8Array[] {
    return this.ctx.storage.sql
      .exec<BytesRow>(
        "SELECT bytes FROM pack_upload_manifest_parts WHERE pack_id = ? ORDER BY position",
        id,
      )
      .toArray()
      .map((row) => new Uint8Array(row.bytes));
  }

  private installedManifestParts(id: ArrayBuffer): Uint8Array[] {
    return this.ctx.storage.sql
      .exec<BytesRow>(
        "SELECT bytes FROM installed_pack_manifest_parts WHERE pack_id = ? ORDER BY position",
        id,
      )
      .toArray()
      .map((row) => new Uint8Array(row.bytes));
  }

  private chunkParts(id: ArrayBuffer, position: number): Uint8Array[] {
    return this.ctx.storage.sql
      .exec<BytesRow>(
        `SELECT bytes FROM pack_upload_chunk_parts
         WHERE pack_id = ? AND chunk_position = ? ORDER BY part_position`,
        id,
        position,
      )
      .toArray()
      .map((row) => new Uint8Array(row.bytes));
  }

  private resetTrappedKernel(error: unknown) {
    if (error instanceof WebAssembly.RuntimeError) this.kernel.reset();
  }
}

function errorResponse(status: number, message: string): Response {
  return Response.json({ error: message }, { status });
}

async function tokenMatches(provided: string, expected: string): Promise<boolean> {
  const encoder = new TextEncoder();
  const [providedHash, expectedHash] = await Promise.all([
    crypto.subtle.digest("SHA-256", encoder.encode(provided)),
    crypto.subtle.digest("SHA-256", encoder.encode(expected)),
  ]);
  return crypto.subtle.timingSafeEqual(providedHash, expectedHash);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const authorized =
      env.SPIKE_TOKEN &&
      (await tokenMatches(
        request.headers.get("authorization") ?? "",
        `Bearer ${env.SPIKE_TOKEN}`,
      ));
    if (!authorized) return errorResponse(401, "unauthorized");

    const url = new URL(request.url);
    const chunkMatch = /^\/repositories\/([^/]+)\/packs\/([^/]+)\/chunks\/([^/]+)$/.exec(
      url.pathname,
    );
    const packMatch = /^\/repositories\/([^/]+)\/packs\/([^/]+)\/(manifest|install)$/.exec(
      url.pathname,
    );
    const headMatch = /^\/repositories\/([^/]+)\/heads$/.exec(url.pathname);
    const projectionMatch = /^\/repositories\/([^/]+)\/projection$/.exec(url.pathname);
    const hiddenPolicyMatch = /^\/repositories\/([^/]+)\/hidden-policy$/.exec(url.pathname);
    const projectionPushMatch = /^\/repositories\/([^/]+)\/git\/pushes$/.exec(url.pathname);
    const projectionPushActionMatch =
      /^\/repositories\/([^/]+)\/git\/pushes\/([^/]+)\/(claim|confirm|recover|replay)$/.exec(
        url.pathname,
      );
    const packCatalogMatch = /^\/repositories\/([^/]+)\/packs$/.exec(url.pathname);
    const initializeMatch = /^\/repositories\/([^/]+)\/initialize$/.exec(url.pathname);
    const repository =
      chunkMatch?.[1] ??
      packMatch?.[1] ??
      headMatch?.[1] ??
      projectionMatch?.[1] ??
      hiddenPolicyMatch?.[1] ??
      projectionPushMatch?.[1] ??
      projectionPushActionMatch?.[1] ??
      packCatalogMatch?.[1] ??
      initializeMatch?.[1];
    const packId = chunkMatch?.[2] ?? packMatch?.[2];
    if (repository === undefined) return errorResponse(404, "not found");
    if (!REPOSITORY_PATTERN.test(repository)) return errorResponse(400, "invalid repository name");
    if (packId !== undefined && !PACK_ID_PATTERN.test(packId)) {
      return errorResponse(400, "invalid pack ID");
    }
    const stub = env.REPOSITORIES.getByName(repository);

    try {
      if (packMatch?.[3] === "manifest" && request.method === "PUT") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        let bytes: Uint8Array;
        try {
          bytes = await readBoundedBody(request, MAX_MANIFEST_BYTES, "manifest");
        } catch (error) {
          return errorResponse(400, error instanceof Error ? error.message : "invalid manifest body");
        }
        return rpcResponse(await stub.putPackManifest(packId, bytes));
      }
      if (packCatalogMatch !== null && request.method === "GET") {
        const after = url.searchParams.get("after") ?? "0";
        if (!/^(0|[1-9][0-9]*)$/.test(after) || !Number.isSafeInteger(Number(after))) {
          return errorResponse(400, "invalid pack cursor");
        }
        const throughValue = url.searchParams.get("through");
        if (
          throughValue !== null &&
          (!/^(0|[1-9][0-9]*)$/.test(throughValue) || !Number.isSafeInteger(Number(throughValue)))
        ) {
          return errorResponse(400, "invalid pack high-water");
        }
        return rpcResponse(
          await stub.listInstalledPacks(
            url.searchParams.get("incarnation"),
            Number(after),
            throughValue === null ? undefined : Number(throughValue),
          ),
        );
      }
      if (packMatch?.[3] === "manifest" && request.method === "GET") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        return binaryRpcResponse(
          await stub.getInstalledPackManifest(packId, url.searchParams.get("incarnation")),
        );
      }
      if (chunkMatch !== null && request.method === "PUT") {
        if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
        if (!/^(0|[1-9][0-9]*)$/.test(chunkMatch[3])) {
          return errorResponse(400, "invalid chunk position");
        }
        const position = Number(chunkMatch[3]);
        if (!Number.isSafeInteger(position)) return errorResponse(400, "invalid chunk position");
        let bytes: Uint8Array;
        try {
          bytes = await readBoundedBody(request, MAX_CHUNK_BYTES, "chunk");
        } catch (error) {
          return errorResponse(400, error instanceof Error ? error.message : "invalid chunk body");
        }
        return rpcResponse(await stub.putPackChunk(packId, position, bytes));
      }
      if (chunkMatch !== null && request.method === "GET") {
        if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
        if (!/^(0|[1-9][0-9]*)$/.test(chunkMatch[3])) {
          return errorResponse(400, "invalid chunk position");
        }
        const position = Number(chunkMatch[3]);
        if (!Number.isSafeInteger(position)) return errorResponse(400, "invalid chunk position");
        return binaryRpcResponse(
          await stub.getInstalledPackChunk(
            packId,
            position,
            url.searchParams.get("incarnation"),
          ),
        );
      }
      if (packMatch?.[3] === "install" && request.method === "POST") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        return rpcResponse(await stub.installPack(packId));
      }
      if (initializeMatch !== null && request.method === "POST") {
        return rpcResponse(
          await stub.initializeRepository(url.searchParams.get("incarnation")),
        );
      }
      if (headMatch !== null && request.method === "GET") {
        return rpcResponse(await stub.getHeads(url.searchParams.get("incarnation")));
      }
      if (headMatch !== null && request.method === "POST") {
        const decoded = await readJsonBody(request, MAX_HEAD_REQUEST_BYTES, "head request");
        if (decoded instanceof Response) return decoded;
        const body = decoded;
        return rpcResponse(await stub.transactHeads(body));
      }
      if (projectionMatch !== null && request.method === "GET") {
        const after = url.searchParams.get("after") ?? "0";
        const throughValue = url.searchParams.get("through");
        if (
          !/^(0|[1-9][0-9]*)$/.test(after) ||
          !Number.isSafeInteger(Number(after)) ||
          (throughValue !== null &&
            (!/^(0|[1-9][0-9]*)$/.test(throughValue) ||
              !Number.isSafeInteger(Number(throughValue))))
        ) {
          return errorResponse(400, "invalid projection cursor");
        }
        return rpcResponse(
          await stub.getProjection(
            url.searchParams.get("incarnation"),
            Number(after),
            throughValue === null ? undefined : Number(throughValue),
          ),
        );
      }
      if (hiddenPolicyMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "hidden policy request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.mutateHiddenPolicy(body));
      }
      if (projectionPushMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "projection push request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.beginProjectionPush(body));
      }
      if (
        projectionPushActionMatch?.[3] === "replay" &&
        request.method === "GET"
      ) {
        return rpcResponse(
          await stub.getProjectionPushReplay(
            projectionPushActionMatch[2],
            url.searchParams.get("incarnation"),
          ),
        );
      }
      if (projectionPushActionMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "projection push request",
        );
        if (body instanceof Response) return body;
        const batchId = projectionPushActionMatch[2];
        switch (projectionPushActionMatch[3]) {
          case "claim":
            return rpcResponse(await stub.claimProjectionPush(batchId, body));
          case "confirm":
            return rpcResponse(await stub.confirmProjectionPush(batchId, body));
          case "recover":
            return rpcResponse(await stub.recoverProjectionPush(batchId, body));
        }
      }
      return errorResponse(404, "not found");
    } catch (error) {
      console.error("repository Durable Object failed", error);
      return errorResponse(500, "repository storage failed");
    }
  },
} satisfies ExportedHandler<Env>;

function rpcResponse(result: { ok: boolean; error?: string; status?: number }): Response {
  if (!result.ok) {
    return errorResponse(result.status ?? 400, result.error ?? "repository request failed");
  }
  const { ok: _, status: __, ...body } = result;
  return Response.json(body);
}

async function readJsonBody(
  request: Request,
  limit: number,
  label: string,
): Promise<unknown | Response> {
  let bytes: Uint8Array;
  try {
    bytes = await readBoundedBody(request, limit, label);
  } catch (error) {
    return errorResponse(400, error instanceof Error ? error.message : `invalid ${label}`);
  }
  try {
    return JSON.parse(
      new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes),
    );
  } catch {
    return errorResponse(400, `${label} must be valid JSON`);
  }
}

function binaryRpcResponse(result: {
  ok: boolean;
  error?: string;
  status?: number;
  bytes?: ArrayBuffer;
}): Response {
  if (!result.ok) {
    return errorResponse(result.status ?? 400, result.error ?? "repository request failed");
  }
  if (result.bytes === undefined) throw new Error("binary repository response is missing bytes");
  return new Response(result.bytes, { headers: { "content-type": "application/octet-stream" } });
}
