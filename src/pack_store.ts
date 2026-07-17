import { HeadStore } from "./head_store";
import { Kernel, equalBytes, exactBuffer, fromHex, toHex } from "./kernel";
import {
  MAX_CHUNK_BYTES,
  MAX_MANIFEST_BYTES,
  MAX_OBJECT_BYTES,
  PackManifest,
  concatParts,
  decodeManifest,
  splitParts,
} from "./pack_protocol";
import { decodeObjectInventory, InventoryObject } from "./object_protocol";
const PACK_CATALOG_PAGE = 256;
const INVENTORY_SQL_OBJECTS = 32;

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

export class PackStore {
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly sql: SqlStorage,
    private readonly kernel: Kernel,
    private readonly heads: HeadStore,
  ) {}

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
          ? this.sql
              .exec<ChunkRow>(
                `SELECT position, byte_offset, byte_length, hash, received
                 FROM pack_upload_chunks WHERE pack_id = ? AND position = ?`,
                id,
                position,
              )
              .toArray()[0]
          : this.sql
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
          this.sql.exec(
            "INSERT INTO pack_upload_chunk_parts VALUES (?, ?, ?, ?)",
            id,
            position,
            partPosition,
            exactBuffer(part),
          );
        }
        this.sql.exec(
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
    const missing = this.sql
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
    return this.sql.exec<{ count: number }>("SELECT count(*) AS count FROM objects").one()
      .count;
  }

  countInstalledPacks(): number {
    return this.sql
      .exec<{ count: number }>("SELECT count(*) AS count FROM installed_packs")
      .one().count;
  }

  /**
   * Returns the installed subset of one bounded, canonical candidate set.
   *
   * Installed objects are immutable and content-addressed, so an affirmative
   * answer remains safe if another pack installs concurrently. A stale
   * negative only causes an idempotent re-upload.
   */
  inventoryObjects(value: unknown) {
    let request;
    try {
      request = decodeObjectInventory(value);
    } catch (error) {
      return {
        ok: false as const,
        status: 400,
        error: error instanceof Error ? error.message : "object inventory request failed",
      };
    }
    const state = this.heads.get(toHex(request.incarnation));
    if (!state.ok) return state;

    const installed: InventoryObject[] = [];
    for (let offset = 0; offset < request.objects.length; offset += INVENTORY_SQL_OBJECTS) {
      const candidates = request.objects.slice(offset, offset + INVENTORY_SQL_OBJECTS);
      const predicate = candidates.map(() => "(kind = ? AND id = ?)").join(" OR ");
      const bindings = candidates.flatMap((object) => [object.kind, exactBuffer(object.id)]);
      installed.push(
        ...this.sql
          .exec<{ kind: number; id: ArrayBuffer }>(
            `SELECT kind, id FROM objects WHERE ${predicate} ORDER BY kind, id`,
            ...bindings,
          )
          .toArray()
          .map((row) => ({ kind: row.kind, id: new Uint8Array(row.id) })),
      );
    }
    return {
      ok: true as const,
      objects: installed.map((object) => ({ kind: object.kind, id: toHex(object.id) })),
    };
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
    const rows = this.sql
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
    const chunk = this.sql
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
    const objects = this.sql
      .exec<InstalledObjectBytesRow>(
        `SELECT indexed.byte_offset, indexed.byte_length, objects.bytes
         FROM installed_pack_objects AS indexed
         JOIN objects ON objects.kind = indexed.kind AND objects.id = indexed.id
         WHERE indexed.pack_id = ?
           AND indexed.byte_length > 0
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
      this.sql.exec(
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
        this.sql.exec(
          "INSERT INTO pack_upload_manifest_parts VALUES (?, ?, ?)",
          id,
          position,
          exactBuffer(part),
        );
      }
      for (const [position, head] of manifest.operationHeads.entries()) {
        this.sql.exec(
          "INSERT INTO pack_upload_heads VALUES (?, ?, ?)",
          id,
          position,
          exactBuffer(head),
        );
      }
      for (const [position, object] of manifest.objects.entries()) {
        this.sql.exec(
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
        this.sql.exec(
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
    const objects = this.sql
      .exec<ObjectRow>(
        `SELECT position, kind, id, byte_offset, byte_length
         FROM pack_upload_objects WHERE pack_id = ? ORDER BY position`,
        id,
      )
      .toArray();
    const chunks = this.sql
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

    this.sql.exec(
      "INSERT INTO installed_packs VALUES (?, ?, ?, ?, ?, ?, ?)",
      id,
      upload.manifest_length,
      upload.pack_length,
      upload.pack_hash,
      upload.chunk_bytes,
      upload.object_count,
      upload.chunk_count,
    );
    this.sql.exec(
      "INSERT INTO installed_pack_catalog VALUES (?, ?)",
      id,
      this.nextInstalledPackSequence(),
    );
    this.sql.exec(
      `INSERT INTO installed_pack_manifest_parts
       SELECT pack_id, position, bytes FROM pack_upload_manifest_parts WHERE pack_id = ?`,
      id,
    );
    this.sql.exec(
      "INSERT INTO installed_pack_heads SELECT pack_id, position, id FROM pack_upload_heads WHERE pack_id = ?",
      id,
    );
    this.sql.exec(
      `INSERT INTO installed_pack_objects
       SELECT pack_id, position, kind, id, byte_offset, byte_length
       FROM pack_upload_objects WHERE pack_id = ?`,
      id,
    );
    this.sql.exec(
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
      this.sql.exec(`DELETE FROM ${table} WHERE pack_id = ?`, id);
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
    const existing = this.sql
      .exec<{ bytes: ArrayBuffer }>("SELECT bytes FROM objects WHERE kind = ? AND id = ?", kind, id)
      .toArray();
    if (existing.length !== 0) {
      if (!equalBytes(new Uint8Array(existing[0].bytes), bytes)) {
        throw new Error("content ID collision with different canonical bytes");
      }
      return false;
    }
    this.sql.exec(
      "INSERT INTO objects (kind, id, bytes) VALUES (?, ?, ?)",
      kind,
      id,
      exactBuffer(bytes),
    );
    for (const reference of references) {
      this.sql.exec(
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
    return this.sql
      .exec<UploadRow>(
        `SELECT manifest_length, pack_length, pack_hash, chunk_bytes, object_count, chunk_count
         FROM pack_uploads WHERE pack_id = ?`,
        id,
      )
      .toArray()[0];
  }

  private installed(id: ArrayBuffer): UploadRow | undefined {
    return this.sql
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
    return this.sql
      .exec<{ sequence: number }>(
        "SELECT COALESCE(MAX(sequence), 0) AS sequence FROM installed_pack_catalog",
      )
      .one().sequence;
  }

  private manifestParts(id: ArrayBuffer): Uint8Array[] {
    return this.sql
      .exec<BytesRow>(
        "SELECT bytes FROM pack_upload_manifest_parts WHERE pack_id = ? ORDER BY position",
        id,
      )
      .toArray()
      .map((row) => new Uint8Array(row.bytes));
  }

  private installedManifestParts(id: ArrayBuffer): Uint8Array[] {
    return this.sql
      .exec<BytesRow>(
        "SELECT bytes FROM installed_pack_manifest_parts WHERE pack_id = ? ORDER BY position",
        id,
      )
      .toArray()
      .map((row) => new Uint8Array(row.bytes));
  }

  private chunkParts(id: ArrayBuffer, position: number): Uint8Array[] {
    return this.sql
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
