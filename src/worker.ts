import { DurableObject } from "cloudflare:workers";
import kernelModule from "../dist/kernel.wasm";

const MAX_OBJECT_BYTES = 1024 * 1024;
const REPOSITORY_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;

const KIND = {
  file: 0,
  symlink: 1,
  tree: 2,
  commit: 3,
  view: 4,
  operation: 5,
} as const;

const KIND_BY_NUMBER = ["file", "symlink", "tree", "commit", "view", "operation"] as const;

type KindName = keyof typeof KIND;

function isKindName(value: string): value is KindName {
  return Object.hasOwn(KIND, value);
}

interface Env {
  REPOSITORIES: DurableObjectNamespace<Repository>;
  SPIKE_TOKEN: string;
}

interface KernelExports extends WebAssembly.Exports {
  memory: WebAssembly.Memory;
  kernel_alloc(length: number): number;
  kernel_dealloc(pointer: number, length: number): void;
  kernel_validate(kind: number, pointer: number, length: number): bigint;
}

interface KernelResult {
  id: Uint8Array;
  references: Array<{ kind: number; id: Uint8Array }>;
}

class Kernel {
  readonly exports: KernelExports;

  constructor() {
    this.exports = new WebAssembly.Instance(kernelModule, {}).exports as KernelExports;
  }

  validate(kind: number, bytes: Uint8Array): KernelResult {
    const inputPointer = this.exports.kernel_alloc(bytes.byteLength);
    try {
      new Uint8Array(this.exports.memory.buffer, inputPointer, bytes.byteLength).set(bytes);
      const packed = this.exports.kernel_validate(kind, inputPointer, bytes.byteLength);
      const outputPointer = Number(packed & 0xffff_ffffn);
      const outputLength = Number(packed >> 32n);
      try {
        const output = new Uint8Array(
          this.exports.memory.buffer,
          outputPointer,
          outputLength,
        ).slice();
        return decodeKernelResult(output);
      } finally {
        this.exports.kernel_dealloc(outputPointer, outputLength);
      }
    } finally {
      this.exports.kernel_dealloc(inputPointer, bytes.byteLength);
    }
  }
}

function decodeKernelResult(bytes: Uint8Array): KernelResult {
  if (bytes[0] === 1) {
    throw new Error(new TextDecoder().decode(bytes.subarray(1)));
  }
  if (bytes[0] !== 0 || bytes.byteLength < 69) {
    throw new Error("validation kernel returned a malformed response");
  }
  const count = new DataView(bytes.buffer, bytes.byteOffset + 65, 4).getUint32(0, true);
  if (bytes.byteLength !== 69 + count * 65) {
    throw new Error("validation kernel returned malformed references");
  }
  const references = [];
  for (let index = 0; index < count; index += 1) {
    const offset = 69 + index * 65;
    const kind = bytes[offset];
    if (KIND_BY_NUMBER[kind] === undefined) {
      throw new Error("validation kernel returned an unknown reference kind");
    }
    references.push({ kind, id: bytes.slice(offset + 1, offset + 65) });
  }
  return { id: bytes.slice(1, 65), references };
}

export class Repository extends DurableObject<Env> {
  private kernel = new Kernel();

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
    `);
  }

  validateAndStore(kind: string, bytes: Uint8Array) {
    if (bytes.byteLength > MAX_OBJECT_BYTES) {
      return { ok: false as const, error: `object exceeds ${MAX_OBJECT_BYTES} byte limit` };
    }
    if (!isKindName(kind)) {
      return { ok: false as const, error: `unknown object kind: ${kind}` };
    }
    const kindNumber = KIND[kind];
    let validated: KernelResult;
    try {
      validated = this.kernel.validate(kindNumber, bytes);
    } catch (error) {
      if (error instanceof WebAssembly.RuntimeError) {
        // A trap leaves the module's allocator state undefined.
        this.kernel = new Kernel();
      }
      return {
        ok: false as const,
        error: error instanceof Error ? error.message : "object validation failed",
      };
    }
    const id = exactBuffer(validated.id);
    const storedBytes = exactBuffer(bytes);
    let inserted = false;
    this.ctx.storage.transactionSync(() => {
      const existing = this.ctx.storage.sql
        .exec<{ bytes: ArrayBuffer }>(
          "SELECT bytes FROM objects WHERE kind = ? AND id = ?",
          kindNumber,
          id,
        )
        .toArray();
      if (existing.length === 0) {
        this.ctx.storage.sql.exec(
          "INSERT INTO objects (kind, id, bytes) VALUES (?, ?, ?)",
          kindNumber,
          id,
          storedBytes,
        );
        for (const reference of validated.references) {
          this.ctx.storage.sql.exec(
            "INSERT INTO object_references VALUES (?, ?, ?, ?)",
            kindNumber,
            id,
            reference.kind,
            exactBuffer(reference.id),
          );
        }
        inserted = true;
      } else if (!equalBytes(new Uint8Array(existing[0].bytes), bytes)) {
        throw new Error("content ID collision with different canonical bytes");
      }
    });
    return {
      ok: true as const,
      id: toHex(validated.id),
      inserted,
      references: validated.references.map((reference) => ({
        kind: KIND_BY_NUMBER[reference.kind],
        id: toHex(reference.id),
      })),
    };
  }

  countObjects(): number {
    return this.ctx.storage.sql.exec<{ count: number }>("SELECT count(*) AS count FROM objects").one()
      .count;
  }
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  return left.byteLength === right.byteLength && left.every((byte, index) => byte === right[index]);
}

function exactBuffer(bytes: Uint8Array): ArrayBuffer {
  return new Uint8Array(bytes).buffer;
}

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function readBoundedBody(request: Request): Promise<Uint8Array> {
  const declaredLength = request.headers.get("content-length");
  if (declaredLength !== null && Number(declaredLength) > MAX_OBJECT_BYTES) {
    throw new Error(`object exceeds ${MAX_OBJECT_BYTES} byte limit`);
  }
  if (request.body === null) {
    return new Uint8Array();
  }
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    length += value.byteLength;
    if (length > MAX_OBJECT_BYTES) {
      await reader.cancel();
      throw new Error(`object exceeds ${MAX_OBJECT_BYTES} byte limit`);
    }
    chunks.push(value);
  }
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
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
    if (!authorized) {
      return errorResponse(401, "unauthorized");
    }
    const url = new URL(request.url);
    const match = /^\/repositories\/([^/]+)\/objects\/([^/]+)$/.exec(url.pathname);
    if (request.method !== "PUT" || match === null) {
      return errorResponse(404, "not found");
    }
    const repository = match[1];
    const kind = match[2];
    if (!REPOSITORY_PATTERN.test(repository)) {
      return errorResponse(400, "invalid repository name");
    }
    if (!isKindName(kind)) {
      return errorResponse(400, `unknown object kind: ${kind}`);
    }
    let bytes: Uint8Array;
    try {
      bytes = await readBoundedBody(request);
    } catch (error) {
      const message = error instanceof Error ? error.message : "invalid request body";
      return errorResponse(400, message);
    }
    try {
      const stub = env.REPOSITORIES.getByName(repository);
      const result = await stub.validateAndStore(kind, bytes);
      if (!result.ok) return errorResponse(400, result.error);
      const { ok: _, ...body } = result;
      return Response.json(body);
    } catch (error) {
      console.error("repository Durable Object failed", error);
      return errorResponse(500, "repository storage failed");
    }
  },
} satisfies ExportedHandler<Env>;
