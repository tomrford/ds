import { canonicalHeadTransactionBytes, decodeHeadTransaction } from "./op_protocol";
import {
  Kernel,
  OP_REFERENCE_KIND,
  equalGitBytes,
  exactGitBuffer,
  gitToHex,
} from "./kernel";

const OP_OBJECT_ID_BYTES = 64;
const MAX_OPERATION_HEADS = 4_096;
const RECEIPT_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;
const MAX_RECEIPTS = 65_536;
const MAX_RECEIPT_HEADS = 1_048_576;
const PRUNE_BATCH = 256;
export const MAX_OP_OBJECT_BYTES = 1024 * 1024;
export const MAX_OP_INVENTORY_KEYS = 4_096;
export const MAX_OP_INVENTORY_REQUEST_BYTES = 640 * 1024;

const OP_KIND = {
  view: 0,
  operation: 1,
} as const;

interface HeadRow extends Record<string, SqlStorageValue> {
  id: ArrayBuffer;
}

interface RepositoryStateRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  op_cursor: number;
  op_receipt_count: number;
  op_receipt_head_count: number;
}

interface ReceiptRow extends Record<string, SqlStorageValue> {
  request_hash: ArrayBuffer;
  cursor: number;
}

interface ReceiptKeyRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  idempotency_key: ArrayBuffer;
}

class OpStoreError extends Error {
  constructor(
    message: string,
    readonly status = 400,
    readonly code = "invalid-op-request",
  ) {
    super(message);
  }
}

export class OpGitStore {
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly sql: SqlStorage,
    private readonly kernel: Kernel,
  ) {}

  put(kindName: "view" | "operation", idValue: unknown, bytes: Uint8Array) {
    try {
      if (bytes.byteLength > MAX_OP_OBJECT_BYTES) {
        throw new OpStoreError(`operation-store object exceeds ${MAX_OP_OBJECT_BYTES} byte limit`);
      }
      const id = decodeId(idValue, "operation-store object ID");
      const validated =
        kindName === "view"
          ? this.kernel.validateView(bytes)
          : this.kernel.validateOperation(bytes);
      if (!equalGitBytes(id, validated.id)) {
        throw new OpStoreError("operation-store object ID does not match canonical bytes");
      }
      const inserted = this.ctx.storage.transactionSync(() => {
        const kind = OP_KIND[kindName];
        const existing = this.sql
          .exec<{ bytes: ArrayBuffer }>(
            "SELECT bytes FROM op_objects WHERE kind = ? AND id = ?",
            kind,
            exactGitBuffer(id),
          )
          .toArray()[0];
        if (existing !== undefined) {
          if (!equalGitBytes(new Uint8Array(existing.bytes), bytes)) {
            throw new Error("content-addressed operation-store object was clobbered");
          }
          return false;
        }
        this.sql.exec(
          "INSERT INTO op_objects (kind, id, bytes) VALUES (?, ?, ?)",
          kind,
          exactGitBuffer(id),
          exactGitBuffer(bytes),
        );
        for (const reference of validated.references) {
          this.sql.exec(
            `INSERT INTO op_object_references
             (object_kind, object_id, reference_kind, referenced_id)
             VALUES (?, ?, ?, ?)`,
            kind,
            exactGitBuffer(id),
            reference.kind,
            exactGitBuffer(reference.id),
          );
        }
        return true;
      });
      return { ok: true as const, inserted };
    } catch (error) {
      return this.failure(error);
    }
  }

  get(kindName: "view" | "operation", idValue: unknown) {
    try {
      const id = decodeId(idValue, "operation-store object ID");
      const row = this.sql
        .exec<{ bytes: ArrayBuffer }>(
          "SELECT bytes FROM op_objects WHERE kind = ? AND id = ?",
          OP_KIND[kindName],
          exactGitBuffer(id),
        )
        .toArray()[0];
      if (row === undefined) {
        throw new OpStoreError("operation-store object was not found", 404, "op-object-not-found");
      }
      return { ok: true as const, bytes: row.bytes };
    } catch (error) {
      return this.failure(error);
    }
  }

  inventory(value: unknown) {
    try {
      const keys = decodeInventory(value);
      const present: string[] = [];
      for (const key of keys) {
        const [prefix, idHex] = key.split(":");
        const kind = prefix === "v" ? OP_KIND.view : OP_KIND.operation;
        const id = decodeId(idHex, "operation-store inventory ID");
        const found = this.sql
          .exec<{ present: number }>(
            "SELECT 1 AS present FROM op_objects WHERE kind = ? AND id = ?",
            kind,
            exactGitBuffer(id),
          )
          .toArray()[0];
        if (found !== undefined) present.push(key);
      }
      return { ok: true as const, keys: present };
    } catch (error) {
      return this.failure(error);
    }
  }

  getHeads(incarnationValue: unknown) {
    try {
      const state = this.requireState(incarnationValue);
      return {
        ok: true as const,
        cursor: state.op_cursor,
        heads: this.currentHeads().map((head) => gitToHex(new Uint8Array(head))),
      };
    } catch (error) {
      return this.failure(error);
    }
  }

  transactHeads(value: unknown) {
    try {
      const request = decodeHeadTransaction(value);
      const incarnation = exactGitBuffer(request.incarnation);
      const idempotencyKey = exactGitBuffer(request.idempotencyKey);
      const requestHash = exactGitBuffer(
        this.kernel.hash([canonicalHeadTransactionBytes(request)]),
      );
      const nowMs = Date.now();
      this.ctx.storage.transactionSync(() => {
        this.requireState(gitToHex(request.incarnation));
        this.pruneExpiredReceipts(nowMs - RECEIPT_RETENTION_MS);
      });
      return this.ctx.storage.transactionSync(() => {
        const currentState = this.requireState(gitToHex(request.incarnation));
        const previous = this.sql
          .exec<ReceiptRow>(
            `SELECT request_hash, cursor FROM op_head_transactions
             WHERE incarnation = ? AND idempotency_key = ?`,
            incarnation,
            idempotencyKey,
          )
          .toArray()[0];
        if (previous !== undefined) {
          if (!equalGitBytes(new Uint8Array(previous.request_hash), new Uint8Array(requestHash))) {
            throw new OpStoreError(
              "idempotency key was already used for a different head request",
              409,
              "head-replay-mismatch",
            );
          }
          return {
            ok: true as const,
            cursor: previous.cursor,
            heads: this.receiptHeads(incarnation, idempotencyKey),
          };
        }

        const newHead = exactGitBuffer(request.newHead);
        this.requireCompleteClosure(newHead);
        const unrelated = this.observedOutsideAncestry(newHead, request.observedHeads);
        if (unrelated !== undefined) {
          throw new OpStoreError(
            `observed current head is not an ancestor of new head: ${gitToHex(new Uint8Array(unrelated))}`,
            409,
            "head-observation-stale",
          );
        }
        for (const observed of request.observedHeads) {
          this.sql.exec("DELETE FROM op_heads WHERE id = ?", exactGitBuffer(observed));
        }
        this.sql.exec("INSERT OR IGNORE INTO op_heads VALUES (?)", newHead);
        const heads = this.currentHeads();
        if (heads.length > MAX_OPERATION_HEADS) {
          throw new OpStoreError(
            `resulting head set exceeds the ${MAX_OPERATION_HEADS}-head limit`,
            409,
            "head-set-limit",
          );
        }
        if (currentState.op_cursor >= Number.MAX_SAFE_INTEGER) {
          throw new Error("repository operation cursor exceeds the safe integer range");
        }
        this.requireReceiptCapacity(currentState, heads.length);
        const cursor = currentState.op_cursor + 1;
        this.sql.exec(
          `INSERT INTO op_head_transactions
           (incarnation, idempotency_key, request_hash, cursor, created_at_ms)
           VALUES (?, ?, ?, ?, ?)`,
          incarnation,
          idempotencyKey,
          requestHash,
          cursor,
          nowMs,
        );
        for (const [position, head] of heads.entries()) {
          this.sql.exec(
            `INSERT INTO op_head_transaction_heads
             (incarnation, idempotency_key, position, id) VALUES (?, ?, ?, ?)`,
            incarnation,
            idempotencyKey,
            position,
            head,
          );
        }
        this.sql.exec(
          `UPDATE repository_state
           SET op_cursor = ?,
               op_receipt_count = op_receipt_count + 1,
               op_receipt_head_count = op_receipt_head_count + ?
           WHERE singleton = 1`,
          cursor,
          heads.length,
        );
        return {
          ok: true as const,
          cursor,
          heads: heads.map((head) => gitToHex(new Uint8Array(head))),
        };
      });
    } catch (error) {
      return this.failure(error);
    }
  }

  countObjects() {
    return this.sql.exec<{ count: number }>("SELECT count(*) AS count FROM op_objects").one().count;
  }

  private requireState(incarnationValue: unknown): RepositoryStateRow {
    const incarnation = decodeShortId(incarnationValue, "incarnation");
    const state = this.sql
      .exec<RepositoryStateRow>(
        `SELECT incarnation, op_cursor, op_receipt_count, op_receipt_head_count
         FROM repository_state WHERE singleton = 1`,
      )
      .one();
    if (!equalGitBytes(new Uint8Array(state.incarnation), incarnation)) {
      throw new OpStoreError(
        "repository incarnation does not match",
        409,
        "repository-incarnation-mismatch",
      );
    }
    return state;
  }

  private pruneExpiredReceipts(cutoffMs: number) {
    const expired = this.sql
      .exec<ReceiptKeyRow>(
        `SELECT incarnation, idempotency_key
         FROM op_head_transactions
         WHERE created_at_ms < ?
         ORDER BY created_at_ms
         LIMIT ${PRUNE_BATCH}`,
        cutoffMs,
      )
      .toArray();
    let removedHeads = 0;
    for (const receipt of expired) {
      removedHeads += this.sql
        .exec<{ count: number }>(
          `SELECT count(*) AS count FROM op_head_transaction_heads
           WHERE incarnation = ? AND idempotency_key = ?`,
          receipt.incarnation,
          receipt.idempotency_key,
        )
        .one().count;
      this.sql.exec(
        `DELETE FROM op_head_transaction_heads
         WHERE incarnation = ? AND idempotency_key = ?`,
        receipt.incarnation,
        receipt.idempotency_key,
      );
      this.sql.exec(
        `DELETE FROM op_head_transactions
         WHERE incarnation = ? AND idempotency_key = ?`,
        receipt.incarnation,
        receipt.idempotency_key,
      );
    }
    if (expired.length !== 0) {
      this.sql.exec(
        `UPDATE repository_state
         SET op_receipt_count = op_receipt_count - ?,
             op_receipt_head_count = op_receipt_head_count - ?
         WHERE singleton = 1`,
        expired.length,
        removedHeads,
      );
    }
  }

  private requireReceiptCapacity(state: RepositoryStateRow, newHeadCount: number) {
    if (
      state.op_receipt_count >= MAX_RECEIPTS ||
      state.op_receipt_head_count + newHeadCount > MAX_RECEIPT_HEADS
    ) {
      throw new OpStoreError(
        "operation head receipt quota is exhausted",
        429,
        "head-receipt-limit",
      );
    }
  }

  private currentHeads(): ArrayBuffer[] {
    return this.sql
      .exec<HeadRow>("SELECT id FROM op_heads ORDER BY id")
      .toArray()
      .map((row) => row.id);
  }

  private receiptHeads(incarnation: ArrayBuffer, idempotencyKey: ArrayBuffer): string[] {
    return this.sql
      .exec<HeadRow>(
        `SELECT id FROM op_head_transaction_heads
         WHERE incarnation = ? AND idempotency_key = ? ORDER BY position`,
        incarnation,
        idempotencyKey,
      )
      .toArray()
      .map((row) => gitToHex(new Uint8Array(row.id)));
  }

  private observedOutsideAncestry(
    newHead: ArrayBuffer,
    observedHeads: Uint8Array[],
  ): ArrayBuffer | undefined {
    for (const observed of observedHeads) {
      const current = this.sql
        .exec<HeadRow>("SELECT id FROM op_heads WHERE id = ?", exactGitBuffer(observed))
        .toArray()[0];
      if (current === undefined) continue;
      const ancestor = this.sql
        .exec<{ id: ArrayBuffer }>(
          `WITH RECURSIVE ancestors(id) AS (
             VALUES (?)
             UNION
             SELECT edges.referenced_id
             FROM ancestors
             JOIN op_object_references AS edges
               ON edges.object_kind = ${OP_KIND.operation}
              AND edges.object_id = ancestors.id
              AND edges.reference_kind = ${OP_REFERENCE_KIND.operation}
           )
           SELECT id FROM ancestors WHERE id = ? LIMIT 1`,
          newHead,
          exactGitBuffer(observed),
        )
        .toArray()[0];
      if (ancestor === undefined) return current.id;
    }
    return undefined;
  }

  private requireCompleteClosure(newHead: ArrayBuffer) {
    const missing = this.sql
      .exec<{ kind: number; id: ArrayBuffer }>(
        `WITH RECURSIVE reachable(kind, id) AS (
           VALUES (${OP_REFERENCE_KIND.operation}, ?)
           UNION
           SELECT edges.reference_kind, edges.referenced_id
           FROM reachable
           JOIN op_object_references AS edges
             ON (reachable.kind = ${OP_REFERENCE_KIND.view}
                 AND edges.object_kind = ${OP_KIND.view}
                 AND edges.object_id = reachable.id)
              OR (reachable.kind = ${OP_REFERENCE_KIND.operation}
                 AND edges.object_kind = ${OP_KIND.operation}
                 AND edges.object_id = reachable.id)
         )
         SELECT reachable.kind, reachable.id
         FROM reachable
         LEFT JOIN objects AS commits
           ON reachable.kind = ${OP_REFERENCE_KIND.commit}
          AND commits.kind = 2
          AND commits.id = reachable.id
         LEFT JOIN op_objects AS views
           ON reachable.kind = ${OP_REFERENCE_KIND.view}
          AND views.kind = ${OP_KIND.view}
          AND views.id = reachable.id
         LEFT JOIN op_objects AS operations
           ON reachable.kind = ${OP_REFERENCE_KIND.operation}
          AND operations.kind = ${OP_KIND.operation}
          AND operations.id = reachable.id
         WHERE commits.id IS NULL AND views.id IS NULL AND operations.id IS NULL
           AND NOT (
             (reachable.kind = ${OP_REFERENCE_KIND.operation}
              AND reachable.id = zeroblob(${OP_OBJECT_ID_BYTES}))
             OR
             (reachable.kind = ${OP_REFERENCE_KIND.commit}
              AND reachable.id = zeroblob(20))
           )
         ORDER BY reachable.kind, reachable.id
         LIMIT 1`,
        newHead,
      )
      .toArray()[0];
    if (missing !== undefined) {
      const label = ["commit", "view", "operation"][missing.kind] ?? `kind ${missing.kind}`;
      throw new OpStoreError(
        `head closure is missing ${label} ${gitToHex(new Uint8Array(missing.id))}`,
        409,
        "head-closure-incomplete",
      );
    }
  }

  private failure(error: unknown) {
    if (error instanceof WebAssembly.RuntimeError) this.kernel.reset();
    return {
      ok: false as const,
      status: error instanceof OpStoreError ? error.status : 400,
      error: error instanceof Error ? error.message : "operation-store request failed",
      code: error instanceof OpStoreError ? error.code : "invalid-op-request",
    };
  }
}

function decodeId(value: unknown, label: string): Uint8Array {
  if (typeof value !== "string" || !/^[0-9a-f]{128}$/.test(value)) {
    throw new OpStoreError(`${label} must be 128 lowercase hex characters`);
  }
  return decodeHex(value, OP_OBJECT_ID_BYTES);
}

function decodeShortId(value: unknown, label: string): Uint8Array {
  if (typeof value !== "string" || !/^[0-9a-f]{32}$/.test(value)) {
    throw new OpStoreError(`${label} must be 32 lowercase hex characters`);
  }
  return decodeHex(value, 16);
}

function decodeHex(value: string, length: number): Uint8Array {
  return Uint8Array.from({ length }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

function decodeInventory(value: unknown): string[] {
  if (
    typeof value !== "object" ||
    value === null ||
    Array.isArray(value) ||
    Object.keys(value).length !== 1 ||
    !("keys" in value) ||
    !Array.isArray(value.keys) ||
    value.keys.length > MAX_OP_INVENTORY_KEYS
  ) {
    throw new OpStoreError("operation-store inventory request must contain only a bounded keys array");
  }
  const keys = value.keys;
  for (const [index, key] of keys.entries()) {
    if (typeof key !== "string" || !/^[vo]:[0-9a-f]{128}$/.test(key)) {
      throw new OpStoreError("operation-store inventory key is invalid");
    }
    if (index > 0 && keys[index - 1] >= key) {
      throw new OpStoreError("operation-store inventory keys must be strictly sorted");
    }
  }
  return keys;
}
