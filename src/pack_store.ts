import {
  GIT_OBJECT_KIND,
  GIT_REFERENCE_KIND,
  Kernel,
  equalGitBytes,
  exactGitBuffer,
  gitHashFromHex,
  gitToHex,
} from "./kernel";
import {
  MAX_GIT_MANIFEST_BYTES,
  MAX_GIT_OBJECT_BYTES,
  GitPackManifest,
  concatGitParts,
  decodeGitManifest,
  splitGitParts,
} from "./pack_protocol";

const PACK_CATALOG_PAGE = 256;

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

interface MissingReferenceRow extends Record<string, SqlStorageValue> {
  object_kind: number;
  object_id: ArrayBuffer;
  referenced_id: ArrayBuffer;
}

class GitPackValidationError extends Error {}

export class GitPackStore {
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly sql: SqlStorage,
    private readonly kernel: Kernel,
  ) {}

  putPackManifest(packId: string, bytes: Uint8Array) {
    try {
      const id = exactGitBuffer(gitHashFromHex(packId));
      const actualId = this.kernel.hash([bytes]);
      if (!equalGitBytes(actualId, new Uint8Array(id))) {
        throw new GitPackValidationError(`manifest hashes to ${gitToHex(actualId)}, not ${packId}`);
      }
      let manifest: GitPackManifest;
      try {
        manifest = decodeGitManifest(bytes);
      } catch (error) {
        throw new GitPackValidationError(
          error instanceof Error ? error.message : "manifest validation failed",
        );
      }
      const installed = this.installed(id);
      if (installed !== undefined) {
        const installedBytes = concatGitParts(
          this.installedManifestParts(id),
          Math.min(installed.manifest_length, MAX_GIT_MANIFEST_BYTES),
        );
        if (!equalGitBytes(installedBytes, bytes)) {
          throw new Error("installed pack ID collision with different manifest bytes");
        }
        return { ok: true as const, inserted: false, installed: true };
      }
      const existing = this.upload(id);
      if (existing !== undefined) {
        const existingBytes = concatGitParts(
          this.manifestParts(id),
          Math.min(existing.manifest_length, MAX_GIT_MANIFEST_BYTES),
        );
        if (!equalGitBytes(existingBytes, bytes)) {
          throw new Error("pack ID collision with different manifest bytes");
        }
        return { ok: true as const, inserted: false, installed: false };
      }
      this.storeManifest(id, bytes, manifest);
      return { ok: true as const, inserted: true, installed: false };
    } catch (error) {
      this.resetTrappedKernel(error);
      if (error instanceof GitPackValidationError) {
        return { ok: false as const, error: error.message };
      }
      throw error;
    }
  }

  putPackChunk(packId: string, position: number, bytes: Uint8Array) {
    try {
      const id = exactGitBuffer(gitHashFromHex(packId));
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
      if (expected === undefined) throw new GitPackValidationError("unknown pack or chunk position");
      if (bytes.byteLength !== expected.byte_length) {
        throw new GitPackValidationError(
          `chunk ${position} is ${bytes.byteLength} bytes, expected ${expected.byte_length}`,
        );
      }
      const actualHash = this.kernel.hash([bytes]);
      if (!equalGitBytes(actualHash, new Uint8Array(expected.hash))) {
        throw new GitPackValidationError(`chunk ${position} hash does not match manifest`);
      }
      if (installed !== undefined) {
        return { ok: true as const, inserted: false, installed: true };
      }
      if (expected.received !== 0) {
        const existing = concatGitParts(this.chunkParts(id, position), expected.byte_length);
        if (!equalGitBytes(existing, bytes)) {
          throw new Error("chunk hash collision with different bytes");
        }
        return { ok: true as const, inserted: false, installed: false };
      }
      this.ctx.storage.transactionSync(() => {
        for (const [partPosition, part] of splitGitParts(bytes).entries()) {
          this.sql.exec(
            "INSERT INTO pack_upload_chunk_parts VALUES (?, ?, ?, ?)",
            id,
            position,
            partPosition,
            exactGitBuffer(part),
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
      if (error instanceof GitPackValidationError) {
        return { ok: false as const, error: error.message };
      }
      throw error;
    }
  }

  installPack(packId: string) {
    let id: ArrayBuffer;
    try {
      id = exactGitBuffer(gitHashFromHex(packId));
    } catch (error) {
      return { ok: false as const, error: error instanceof Error ? error.message : "invalid pack ID" };
    }
    if (this.installed(id) !== undefined) {
      return { ok: true as const, installed: false, insertedObjects: 0 };
    }
    const upload = this.upload(id);
    if (upload === undefined) return { ok: false as const, error: "pack manifest is not uploaded" };
    const missingChunks = this.sql
      .exec<{ count: number }>(
        "SELECT count(*) AS count FROM pack_upload_chunks WHERE pack_id = ? AND received = 0",
        id,
      )
      .one().count;
    if (missingChunks !== 0) {
      return { ok: false as const, error: `pack is missing ${missingChunks} chunks` };
    }

    try {
      const insertedObjects = this.ctx.storage.transactionSync(() =>
        this.installUploadedPack(id, upload),
      );
      return { ok: true as const, installed: true, insertedObjects };
    } catch (error) {
      this.resetTrappedKernel(error);
      if (error instanceof GitPackValidationError) {
        return { ok: false as const, error: error.message };
      }
      throw error;
    }
  }

  countObjects(): number {
    return this.sql.exec<{ count: number }>("SELECT count(*) AS count FROM objects").one().count;
  }

  countObjectReferences(): number {
    return this.sql
      .exec<{ count: number }>("SELECT count(*) AS count FROM object_references")
      .one().count;
  }

  countInstalledPacks(): number {
    return this.sql
      .exec<{ count: number }>("SELECT count(*) AS count FROM installed_packs")
      .one().count;
  }

  countQuarantinedPacks(): number {
    return this.sql
      .exec<{ count: number }>("SELECT count(*) AS count FROM pack_uploads")
      .one().count;
  }

  listInstalledPacks(afterValue: unknown, throughValue: unknown) {
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
      packs: page.map((row) => ({ sequence: row.sequence, id: gitToHex(new Uint8Array(row.pack_id)) })),
      nextAfter: page.at(-1)?.sequence ?? afterValue,
      through,
      hasMore,
    };
  }

  getInstalledPackManifest(packId: string) {
    let id: ArrayBuffer;
    try {
      id = exactGitBuffer(gitHashFromHex(packId));
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
    const bytes = concatGitParts(
      this.installedManifestParts(id),
      Math.min(installed.manifest_length, MAX_GIT_MANIFEST_BYTES),
    );
    if (bytes.byteLength !== installed.manifest_length) {
      throw new Error("installed pack manifest is incomplete");
    }
    if (!equalGitBytes(this.kernel.hash([bytes]), new Uint8Array(id))) {
      throw new Error("installed pack manifest hash changed");
    }
    return { ok: true as const, bytes: exactGitBuffer(bytes) };
  }

  getInstalledPackChunk(packId: string, position: number) {
    let id: ArrayBuffer;
    try {
      id = exactGitBuffer(gitHashFromHex(packId));
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
    if (filled !== bytes.byteLength) throw new Error("installed pack chunk is incomplete");
    if (!equalGitBytes(this.kernel.hash([bytes]), new Uint8Array(chunk.hash))) {
      throw new Error("installed pack chunk hash changed");
    }
    return { ok: true as const, bytes: exactGitBuffer(bytes) };
  }

  private storeManifest(id: ArrayBuffer, bytes: Uint8Array, manifest: GitPackManifest) {
    this.ctx.storage.transactionSync(() => {
      this.sql.exec(
        "INSERT INTO pack_uploads VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        bytes.byteLength,
        manifest.packLength,
        exactGitBuffer(manifest.packHash),
        manifest.chunkBytes,
        manifest.objects.length,
        manifest.chunks.length,
      );
      for (const [position, part] of splitGitParts(bytes).entries()) {
        this.sql.exec(
          "INSERT INTO pack_upload_manifest_parts VALUES (?, ?, ?)",
          id,
          position,
          exactGitBuffer(part),
        );
      }
      for (const [position, head] of manifest.headCommits.entries()) {
        this.sql.exec(
          "INSERT INTO pack_upload_heads VALUES (?, ?, ?)",
          id,
          position,
          exactGitBuffer(head),
        );
      }
      for (const [position, object] of manifest.objects.entries()) {
        this.sql.exec(
          "INSERT INTO pack_upload_objects VALUES (?, ?, ?, ?, ?, ?)",
          id,
          position,
          object.kind,
          exactGitBuffer(object.id),
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
          exactGitBuffer(chunk.hash),
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
      throw new GitPackValidationError("quarantined pack index is incomplete");
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
          insertedObjects += this.validateAndStoreManifestObject(object, objectBytes!) ? 1 : 0;
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
          throw new GitPackValidationError(`stored chunk ${chunk.position} length changed`);
        }
        const chunkHash = this.kernel.hash(parts);
        if (!equalGitBytes(chunkHash, new Uint8Array(chunk.hash))) {
          throw new GitPackValidationError(`stored chunk ${chunk.position} hash changed`);
        }
        for (const part of parts) {
          packHash.update(part);
          let partOffset = 0;
          while (partOffset < part.byteLength) {
            if (objectIndex >= objects.length || objectBytes === undefined) {
              throw new GitPackValidationError("pack contains bytes outside its object ranges");
            }
            const object = objects[objectIndex];
            if (object.byte_offset + objectWritten !== packOffset) {
              throw new GitPackValidationError("pack object ranges changed after manifest upload");
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
        throw new GitPackValidationError("pack bytes do not fill every object range");
      }
      const actualPackHash = packHash.finish();
      if (!equalGitBytes(actualPackHash, new Uint8Array(upload.pack_hash))) {
        throw new GitPackValidationError("whole-pack hash does not match manifest");
      }
    } finally {
      packHash.dispose();
    }

    this.requirePackClosure(id);
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

  private validateAndStoreManifestObject(object: ObjectRow, bytes: Uint8Array): boolean {
    if (bytes.byteLength > MAX_GIT_OBJECT_BYTES) {
      throw new GitPackValidationError(`object ${object.position} exceeds the object byte limit`);
    }
    let validated;
    try {
      validated = this.kernel.validate(object.kind, bytes);
    } catch (error) {
      if (error instanceof WebAssembly.RuntimeError) throw error;
      throw new GitPackValidationError(
        `object ${object.position} is invalid: ${error instanceof Error ? error.message : "validation failed"}`,
      );
    }
    if (!equalGitBytes(validated.id, new Uint8Array(object.id))) {
      throw new GitPackValidationError(`object ${object.position} ID does not match manifest`);
    }
    const id = exactGitBuffer(validated.id);
    const existing = this.sql
      .exec<{ bytes: ArrayBuffer }>("SELECT bytes FROM objects WHERE kind = ? AND id = ?", object.kind, id)
      .toArray()[0];
    if (existing !== undefined) {
      if (!equalGitBytes(new Uint8Array(existing.bytes), bytes)) {
        throw new Error("content ID collision with different canonical bytes");
      }
      return false;
    }
    this.sql.exec(
      "INSERT INTO objects (kind, id, bytes) VALUES (?, ?, ?)",
      object.kind,
      id,
      exactGitBuffer(bytes),
    );
    for (const reference of validated.references) {
      this.sql.exec(
        "INSERT INTO object_references VALUES (?, ?, ?, ?)",
        object.kind,
        id,
        reference.kind,
        exactGitBuffer(reference.id),
      );
    }
    return true;
  }

  private requirePackClosure(id: ArrayBuffer) {
    const missing = this.sql
      .exec<MissingReferenceRow>(
        `SELECT refs.object_kind, refs.object_id, refs.referenced_id
         FROM object_references AS refs
         JOIN pack_upload_objects AS packed
           ON packed.pack_id = ?
          AND packed.kind = refs.object_kind
          AND packed.id = refs.object_id
         WHERE refs.reference_kind != ?
           AND NOT EXISTS (
             SELECT 1 FROM objects AS target
             WHERE target.kind = CASE refs.reference_kind
               WHEN ? THEN ?
               WHEN ? THEN ?
               WHEN ? THEN ?
               WHEN ? THEN ?
               WHEN ? THEN ?
             END
               AND target.id = refs.referenced_id
           )
         ORDER BY refs.object_kind, refs.object_id, refs.reference_kind, refs.referenced_id
         LIMIT 1`,
        id,
        GIT_REFERENCE_KIND.gitlink,
        GIT_REFERENCE_KIND.blob,
        GIT_OBJECT_KIND.blob,
        GIT_REFERENCE_KIND.executable,
        GIT_OBJECT_KIND.blob,
        GIT_REFERENCE_KIND.symlink,
        GIT_OBJECT_KIND.blob,
        GIT_REFERENCE_KIND.tree,
        GIT_OBJECT_KIND.tree,
        GIT_REFERENCE_KIND.commit,
        GIT_OBJECT_KIND.commit,
      )
      .toArray()[0];
    if (missing !== undefined) {
      throw new GitPackValidationError(
        `object ${gitToHex(new Uint8Array(missing.object_id))} is missing referenced object ${gitToHex(new Uint8Array(missing.referenced_id))}`,
      );
    }
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
