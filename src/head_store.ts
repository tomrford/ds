import {
  HeadTransactionRequest,
  MAX_OPERATION_HEADS,
  canonicalHeadTransactionBytes,
  decodeHeadTransaction,
  decodeIncarnation,
} from "./head_protocol";
import { KIND, KIND_BY_NUMBER, Kernel, equalBytes, exactBuffer, toHex } from "./kernel";
import { RepositoryAuthority } from "./control_plane";

interface RepositoryStateRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  cursor: number;
  receipt_count: number;
  receipt_head_count: number;
}

interface HeadTransactionRow extends Record<string, SqlStorageValue> {
  request_hash: ArrayBuffer;
  cursor: number;
}

interface HeadRow extends Record<string, SqlStorageValue> {
  id: ArrayBuffer;
}

interface MissingObjectRow extends Record<string, SqlStorageValue> {
  kind: number;
  id: ArrayBuffer;
}

interface ReceiptKeyRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  idempotency_key: ArrayBuffer;
}

const RECEIPT_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;
const MAX_RECEIPTS = 65_536;
const MAX_RECEIPT_HEADS = 1_048_576;
const PRUNE_BATCH = 256;

class HeadTransactionError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

export class HeadStore {
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly sql: SqlStorage,
    private readonly kernel: Kernel,
  ) {}

  initialize(authority: RepositoryAuthority) {
    let incarnation: ArrayBuffer;
    try {
      incarnation = exactBuffer(decodeIncarnation(authority.incarnation));
    } catch (error) {
      return failure(error, 400);
    }
    try {
      return this.ctx.storage.transactionSync(() => {
        const state = this.repositoryState();
        if (state === undefined) {
          this.sql.exec(
            `INSERT INTO repository_state
             (singleton, incarnation, user_id, repository_id, retired, cursor,
              receipt_count, receipt_head_count)
             VALUES (1, ?, ?, ?, 0, 0, 0, 0)`,
            incarnation,
            authority.userId,
            authority.repositoryId,
          );
          return { ok: true as const, initialized: true, cursor: 0, heads: [] as string[] };
        }
        this.requireIncarnation(state, incarnation);
        const identity = this.sql
          .exec<{ user_id: string | null; repository_id: string | null; retired: number }>(
            "SELECT user_id, repository_id, retired FROM repository_state WHERE singleton = 1",
          )
          .one();
        if (
          identity.user_id !== authority.userId ||
          identity.repository_id !== authority.repositoryId ||
          identity.retired !== 0
        ) {
          throw new HeadTransactionError("repository authority does not match", 409);
        }
        return {
          ok: true as const,
          initialized: false,
          cursor: state.cursor,
          heads: this.currentHeadHexes(),
        };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  get(incarnationValue: unknown) {
    let incarnation: ArrayBuffer;
    try {
      incarnation = exactBuffer(decodeIncarnation(incarnationValue));
    } catch (error) {
      return failure(error, 400);
    }
    try {
      const state = this.requireRepositoryState();
      this.requireIncarnation(state, incarnation);
      return { ok: true as const, cursor: state.cursor, heads: this.currentHeadHexes() };
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  transact(value: unknown) {
    let request: HeadTransactionRequest;
    try {
      request = decodeHeadTransaction(value);
    } catch (error) {
      return failure(error, 400);
    }
    const incarnation = exactBuffer(request.incarnation);
    const idempotencyKey = exactBuffer(request.idempotencyKey);
    const requestHash = exactBuffer(this.kernel.hash([canonicalHeadTransactionBytes(request)]));
    const nowMs = Date.now();
    try {
      this.ctx.storage.transactionSync(() => {
        const state = this.requireRepositoryState();
        this.requireIncarnation(state, incarnation);
        this.pruneExpiredReceipts(nowMs - RECEIPT_RETENTION_MS);
      });
      return this.ctx.storage.transactionSync(() => {
        const state = this.requireRepositoryState();
        this.requireIncarnation(state, incarnation);
        const previous = this.sql
          .exec<HeadTransactionRow>(
            `SELECT request_hash, cursor FROM head_transactions
             WHERE incarnation = ? AND idempotency_key = ?`,
            incarnation,
            idempotencyKey,
          )
          .toArray()[0];
        if (previous !== undefined) {
          if (!equalBytes(new Uint8Array(previous.request_hash), new Uint8Array(requestHash))) {
            throw new HeadTransactionError(
              "idempotency key was already used for a different head request",
              409,
            );
          }
          return {
            ok: true as const,
            cursor: previous.cursor,
            heads: this.transactionHeadHexes(incarnation, idempotencyKey),
          };
        }

        const newHead = exactBuffer(request.newHead);
        const missing = this.findMissingReachableObject(newHead);
        if (missing !== undefined) {
          throw new HeadTransactionError(
            `head closure is missing ${objectLabel(missing.kind)} ${toHex(new Uint8Array(missing.id))}`,
            409,
          );
        }
        this.sql.exec("DELETE FROM pending_observed_heads");
        for (const observed of request.observedHeads) {
          this.sql.exec(
            "INSERT INTO pending_observed_heads VALUES (?)",
            exactBuffer(observed),
          );
        }
        const unrelated = this.findObservedHeadOutsideAncestry(newHead);
        if (unrelated !== undefined) {
          throw new HeadTransactionError(
            `observed current head is not an ancestor of new head: ${toHex(new Uint8Array(unrelated.id))}`,
            409,
          );
        }
        this.markClosureComplete(newHead);
        for (const observed of request.observedHeads) {
          this.sql.exec(
            "DELETE FROM operation_heads WHERE id = ?",
            exactBuffer(observed),
          );
        }
        this.sql.exec("INSERT OR IGNORE INTO operation_heads VALUES (?)", newHead);
        const headCount = this.sql
          .exec<{ count: number }>("SELECT count(*) AS count FROM operation_heads")
          .one().count;
        if (headCount > MAX_OPERATION_HEADS) {
          throw new HeadTransactionError(
            `resulting head set exceeds the ${MAX_OPERATION_HEADS}-head limit`,
            409,
          );
        }
        if (state.cursor >= Number.MAX_SAFE_INTEGER) {
          throw new Error("repository cursor exceeds the safe integer range");
        }
        const cursor = state.cursor + 1;
        this.sql.exec(
          "UPDATE repository_state SET cursor = ? WHERE singleton = 1",
          cursor,
        );
        const heads = this.currentHeads();
        this.requireReceiptCapacity(state, heads.length);
        this.sql.exec(
          "INSERT INTO head_transactions VALUES (?, ?, ?, ?, ?)",
          incarnation,
          idempotencyKey,
          requestHash,
          cursor,
          nowMs,
        );
        for (const [position, head] of heads.entries()) {
          this.sql.exec(
            "INSERT INTO head_transaction_heads VALUES (?, ?, ?, ?)",
            incarnation,
            idempotencyKey,
            position,
            head,
          );
        }
        this.sql.exec(
          `UPDATE repository_state
           SET receipt_count = receipt_count + 1,
               receipt_head_count = receipt_head_count + ?
           WHERE singleton = 1`,
          heads.length,
        );
        this.sql.exec("DELETE FROM pending_observed_heads");
        return {
          ok: true as const,
          cursor,
          heads: heads.map((head) => toHex(new Uint8Array(head))),
        };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  private repositoryState(): RepositoryStateRow | undefined {
    return this.sql
      .exec<RepositoryStateRow>(
        `SELECT incarnation, cursor, receipt_count, receipt_head_count
         FROM repository_state WHERE singleton = 1`,
      )
      .toArray()[0];
  }

  private pruneExpiredReceipts(cutoffMs: number) {
    const expired = this.sql
      .exec<ReceiptKeyRow>(
        `SELECT incarnation, idempotency_key
         FROM head_transactions
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
          `SELECT count(*) AS count FROM head_transaction_heads
           WHERE incarnation = ? AND idempotency_key = ?`,
          receipt.incarnation,
          receipt.idempotency_key,
        )
        .one().count;
      this.sql.exec(
        `DELETE FROM head_transaction_heads
         WHERE incarnation = ? AND idempotency_key = ?`,
        receipt.incarnation,
        receipt.idempotency_key,
      );
      this.sql.exec(
        `DELETE FROM head_transactions
         WHERE incarnation = ? AND idempotency_key = ?`,
        receipt.incarnation,
        receipt.idempotency_key,
      );
    }
    if (expired.length !== 0) {
      this.sql.exec(
        `UPDATE repository_state
         SET receipt_count = receipt_count - ?,
             receipt_head_count = receipt_head_count - ?
         WHERE singleton = 1`,
        expired.length,
        removedHeads,
      );
    }
  }

  private requireReceiptCapacity(state: RepositoryStateRow, newHeadCount: number) {
    if (
      state.receipt_count >= MAX_RECEIPTS ||
      state.receipt_head_count + newHeadCount > MAX_RECEIPT_HEADS
    ) {
      throw new HeadTransactionError("idempotency receipt quota is exhausted", 429);
    }
  }

  private requireRepositoryState(): RepositoryStateRow {
    const state = this.repositoryState();
    if (state === undefined) throw new HeadTransactionError("repository is not initialized", 409);
    return state;
  }

  private requireIncarnation(state: RepositoryStateRow, incarnation: ArrayBuffer) {
    if (!equalBytes(new Uint8Array(state.incarnation), new Uint8Array(incarnation))) {
      throw new HeadTransactionError("repository incarnation does not match", 409);
    }
  }

  private currentHeads(): ArrayBuffer[] {
    return this.sql
      .exec<HeadRow>("SELECT id FROM operation_heads ORDER BY id")
      .toArray()
      .map((row) => row.id);
  }

  private currentHeadHexes(): string[] {
    return this.currentHeads().map((head) => toHex(new Uint8Array(head)));
  }

  private transactionHeadHexes(incarnation: ArrayBuffer, idempotencyKey: ArrayBuffer): string[] {
    return this.sql
      .exec<HeadRow>(
        `SELECT id FROM head_transaction_heads
         WHERE incarnation = ? AND idempotency_key = ? ORDER BY position`,
        incarnation,
        idempotencyKey,
      )
      .toArray()
      .map((row) => toHex(new Uint8Array(row.id)));
  }

  private findMissingReachableObject(head: ArrayBuffer): MissingObjectRow | undefined {
    return this.sql
      .exec<MissingObjectRow>(
        `WITH RECURSIVE reachable(kind, id) AS (
           VALUES (${KIND.operation}, ?)
           UNION
           SELECT edges.referenced_kind, edges.referenced_id
           FROM reachable
           JOIN object_references AS edges
             ON edges.object_kind = reachable.kind
            AND edges.object_id = reachable.id
           LEFT JOIN complete_object_closures AS complete
             ON complete.kind = reachable.kind AND complete.id = reachable.id
           WHERE complete.id IS NULL
         )
         SELECT reachable.kind, reachable.id
         FROM reachable
         LEFT JOIN objects
           ON objects.kind = reachable.kind AND objects.id = reachable.id
         WHERE objects.id IS NULL
           AND NOT (
             reachable.kind IN (${KIND.commit}, ${KIND.view}, ${KIND.operation})
             AND reachable.id = zeroblob(64)
           )
         ORDER BY reachable.kind, reachable.id
         LIMIT 1`,
        head,
      )
      .toArray()[0];
  }

  private markClosureComplete(head: ArrayBuffer) {
    this.sql.exec(
      `INSERT OR IGNORE INTO complete_object_closures
       WITH RECURSIVE reachable(kind, id) AS (
         VALUES (${KIND.operation}, ?)
         UNION
         SELECT edges.referenced_kind, edges.referenced_id
         FROM reachable
         JOIN object_references AS edges
           ON edges.object_kind = reachable.kind
          AND edges.object_id = reachable.id
         LEFT JOIN complete_object_closures AS complete
           ON complete.kind = reachable.kind AND complete.id = reachable.id
         WHERE complete.id IS NULL
       )
       SELECT kind, id FROM reachable`,
      head,
    );
  }

  private findObservedHeadOutsideAncestry(head: ArrayBuffer): HeadRow | undefined {
    return this.sql
      .exec<HeadRow>(
        `WITH RECURSIVE ancestors(id) AS (
           VALUES (?)
           UNION
           SELECT edges.referenced_id
           FROM ancestors
           JOIN object_references AS edges
             ON edges.object_kind = ${KIND.operation}
            AND edges.object_id = ancestors.id
            AND edges.referenced_kind = ${KIND.operation}
         )
         SELECT observed.id
         FROM pending_observed_heads AS observed
         JOIN operation_heads AS current ON current.id = observed.id
         LEFT JOIN ancestors ON ancestors.id = observed.id
         WHERE ancestors.id IS NULL
         ORDER BY observed.id
         LIMIT 1`,
        head,
      )
      .toArray()[0];
  }

  private handleExpected(error: unknown) {
    if (error instanceof WebAssembly.RuntimeError) this.kernel.reset();
    if (error instanceof HeadTransactionError) return failure(error, error.status);
    throw error;
  }
}

function failure(error: unknown, status: number) {
  return {
    ok: false as const,
    status,
    error: error instanceof Error ? error.message : "head request failed",
  };
}

function objectLabel(kind: number): string {
  return KIND_BY_NUMBER[kind] ?? `kind ${kind}`;
}
