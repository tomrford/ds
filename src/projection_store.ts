import { KIND, KIND_BY_NUMBER, Kernel, equalBytes, exactBuffer, toHex } from "./kernel";
import {
  BeginProjectionBatchRequest,
  MAX_HIDDEN_POLICY_BYTES,
  MAX_HIDDEN_PATHS,
  MAX_PROJECTION_STATES,
  MAX_REPOSITORY_PROJECTION_REFS,
  ProjectionFenceRequest,
  ProjectionObservation,
  canonicalProjectionBatchBytes,
  compareNullableBytes,
  decodeBeginProjectionBatch,
  decodeClaimProjectionBatch,
  decodeHiddenPolicyMutation,
  decodeProjectionFence,
  decodeRecoverProjectionBatch,
} from "./projection_protocol";

interface RepositoryStateRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
}

interface ProjectionMetaRow extends Record<string, SqlStorageValue> {
  current_policy_epoch: number;
  next_fence: number;
  activation_cursor: number;
}

interface BatchRow extends Record<string, SqlStorageValue> {
  remote: string;
  policy_epoch: number;
  owner_machine: ArrayBuffer;
  fence: number;
  request_hash: ArrayBuffer;
}

interface BatchResultRow extends Record<string, SqlStorageValue> {
  request_hash: ArrayBuffer;
  final_fence: number;
  outcome: string;
}

interface BatchRefRow extends Record<string, SqlStorageValue> {
  bookmark: string;
  expected_old_oid: ArrayBuffer | null;
  proposed_state_id: number | null;
  proposed_git_oid: ArrayBuffer | null;
}

interface CursorRow extends Record<string, SqlStorageValue> {
  remote: string;
  bookmark: string;
  git_oid: ArrayBuffer;
  canonical_commit_id: ArrayBuffer;
  public_commit_id: ArrayBuffer;
  policy_epoch: number;
  activation_seq: number;
}

interface PendingRow extends Record<string, SqlStorageValue> {
  batch_id: ArrayBuffer;
  remote: string;
  policy_epoch: number;
  owner_machine: ArrayBuffer;
  fence: number;
}

interface PendingRefSnapshotRow extends Record<string, SqlStorageValue> {
  bookmark: string;
  expected_old_oid: ArrayBuffer | null;
  proposed_git_oid: ArrayBuffer | null;
}

interface ReplayStateRow extends Record<string, SqlStorageValue> {
  state_id: number;
  git_oid: ArrayBuffer;
  canonical_commit_id: ArrayBuffer;
  public_commit_id: ArrayBuffer;
}

interface MappingRow extends Record<string, SqlStorageValue> {
  remote: string;
  bookmark: string;
  git_oid: ArrayBuffer;
  canonical_commit_id: ArrayBuffer;
  public_commit_id: ArrayBuffer;
  policy_epoch: number;
  activation_seq: number;
}

interface ReceiptRow extends Record<string, SqlStorageValue> {
  public_commit_id: ArrayBuffer;
}

interface MissingObjectRow extends Record<string, SqlStorageValue> {
  kind: number;
  id: ArrayBuffer;
}

class ProjectionStoreError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

export class ProjectionStore {
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly sql: SqlStorage,
    private readonly kernel: Kernel,
  ) {}

  get(incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    let incarnation: ArrayBuffer;
    try {
      incarnation = exactBuffer(decodeIncarnationOnly(incarnationValue));
      if (
        typeof afterValue !== "number" ||
        !Number.isSafeInteger(afterValue) ||
        afterValue < 0
      ) {
        throw new ProjectionStoreError("projection cursor must be a non-negative integer", 400);
      }
      this.requireIncarnation(incarnation);
      const meta = this.meta();
      const through = throughValue === undefined ? meta.activation_cursor : throughValue;
      if (
        typeof through !== "number" ||
        !Number.isSafeInteger(through) ||
        through < afterValue ||
        through > meta.activation_cursor
      ) {
        throw new ProjectionStoreError(
          "projection high-water must be between the cursor and current activation frontier",
          400,
        );
      }
      const hiddenPaths = this.sql
        .exec<{ path: string }>(
          "SELECT path FROM hidden_policy_paths WHERE epoch = ? ORDER BY path",
          meta.current_policy_epoch,
        )
        .toArray()
        .map((row) => row.path);
      const cursors = this.sql
        .exec<CursorRow>(
          `SELECT cursors.remote, cursors.bookmark, states.git_oid,
                  states.canonical_commit_id, states.public_commit_id,
                  states.policy_epoch, states.activation_seq
           FROM projection_cursors AS cursors
           JOIN projection_states AS states ON states.state_id = cursors.state_id
           ORDER BY cursors.remote, cursors.bookmark`,
        )
        .toArray()
        .map((row) => ({
          remote: row.remote,
          bookmark: row.bookmark,
          gitOid: toHex(new Uint8Array(row.git_oid)),
          canonicalCommitId: toHex(new Uint8Array(row.canonical_commit_id)),
          publicCommitId: toHex(new Uint8Array(row.public_commit_id)),
          policyEpoch: row.policy_epoch,
          activationSequence: row.activation_seq,
        }));
      const pending = this.sql
        .exec<PendingRow>(
          `SELECT batch_id, remote, policy_epoch, owner_machine, fence
           FROM projection_batches ORDER BY batch_id`,
        )
        .toArray()
        .map((row) => ({
          batchId: toHex(new Uint8Array(row.batch_id)),
          remote: row.remote,
          policyEpoch: row.policy_epoch,
          ownerMachine: toHex(new Uint8Array(row.owner_machine)),
          fence: row.fence,
          refs: this.pendingRefSnapshot(row.batch_id),
        }));
      const mappingRows = this.sql
        .exec<MappingRow>(
          `SELECT remote, bookmark, git_oid, canonical_commit_id,
                  public_commit_id, policy_epoch, activation_seq
           FROM projection_states
           WHERE pending_batch_id IS NULL
             AND activation_seq > ? AND activation_seq <= ?
           ORDER BY activation_seq LIMIT 257`,
          afterValue,
          through,
        )
        .toArray();
      const hasMore = mappingRows.length > 256;
      const pageRows = mappingRows.slice(0, 256);
      const mappings = pageRows
        .map((row) => ({
          remote: row.remote,
          bookmark: row.bookmark,
          gitOid: toHex(new Uint8Array(row.git_oid)),
          canonicalCommitId: toHex(new Uint8Array(row.canonical_commit_id)),
          publicCommitId: toHex(new Uint8Array(row.public_commit_id)),
          policyEpoch: row.policy_epoch,
        }));
      const nextAfter =
        pageRows.length === 0
          ? afterValue
          : pageRows[pageRows.length - 1].activation_seq;
      return {
        ok: true as const,
        policyEpoch: meta.current_policy_epoch,
        hiddenPaths,
        activationCursor: meta.activation_cursor,
        cursors,
        mappings,
        nextAfter,
        through,
        hasMore,
        pending,
      };
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  private pendingRefSnapshot(batchId: ArrayBuffer) {
    return this.sql
      .exec<PendingRefSnapshotRow>(
        `SELECT refs.bookmark, refs.expected_old_oid, states.git_oid AS proposed_git_oid
         FROM projection_batch_refs AS refs
         LEFT JOIN projection_states AS states ON states.state_id = refs.proposed_state_id
         WHERE refs.batch_id = ? ORDER BY refs.position`,
        batchId,
      )
      .toArray()
      .map((row) => ({
        bookmark: row.bookmark,
        expectedOldOid:
          row.expected_old_oid === null ? null : toHex(new Uint8Array(row.expected_old_oid)),
        proposedGitOid:
          row.proposed_git_oid === null ? null : toHex(new Uint8Array(row.proposed_git_oid)),
      }));
  }

  replay(batchIdValue: unknown, incarnationValue: unknown) {
    let batchId: ArrayBuffer;
    let incarnation: ArrayBuffer;
    try {
      batchId = exactBuffer(decodeBatchId(batchIdValue));
      incarnation = exactBuffer(decodeIncarnationOnly(incarnationValue));
    } catch (error) {
      return failure(error, 400);
    }
    try {
      this.requireIncarnation(incarnation);
      const batch = this.requireBatch(batchId);
      const updates = this.batchRefs(batchId).map((ref) => {
        const states = this.sql
          .exec<ReplayStateRow>(
            `SELECT state_id, git_oid, canonical_commit_id, public_commit_id
             FROM projection_states
             WHERE pending_batch_id = ? AND remote = ? AND bookmark = ?
             ORDER BY state_id`,
            batchId,
            batch.remote,
            ref.bookmark,
          )
          .toArray();
        const proposedState =
          ref.proposed_state_id === null
            ? null
            : states.findIndex((state) => state.state_id === ref.proposed_state_id);
        if (proposedState === -1) {
          throw new Error("pending projection state is missing");
        }
        return {
          bookmark: ref.bookmark,
          expectedOldOid:
            ref.expected_old_oid === null
              ? null
              : toHex(new Uint8Array(ref.expected_old_oid)),
          states: states.map((state) => ({
            gitOid: toHex(new Uint8Array(state.git_oid)),
            canonicalCommitId: toHex(new Uint8Array(state.canonical_commit_id)),
            publicCommitId: toHex(new Uint8Array(state.public_commit_id)),
          })),
          proposedState,
        };
      });
      return {
        ok: true as const,
        batchId: toHex(new Uint8Array(batchId)),
        remote: batch.remote,
        policyEpoch: batch.policy_epoch,
        ownerMachine: toHex(new Uint8Array(batch.owner_machine)),
        fence: batch.fence,
        updates,
      };
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  mutatePolicy(value: unknown) {
    let request: ReturnType<typeof decodeHiddenPolicyMutation>;
    try {
      request = decodeHiddenPolicyMutation(value);
    } catch (error) {
      return failure(error, 400);
    }
    const incarnation = exactBuffer(request.incarnation);
    try {
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(incarnation);
        const meta = this.meta();
        const present =
          this.sql
            .exec<{ count: number }>(
              "SELECT count(*) AS count FROM hidden_policy_paths WHERE epoch = ? AND path = ?",
              meta.current_policy_epoch,
              request.path,
            )
            .one().count !== 0;
        if (present === request.hidden) {
          return {
            ok: true as const,
            changed: false,
            policyEpoch: meta.current_policy_epoch,
          };
        }
        const pending = this.sql
          .exec<{ count: number }>("SELECT count(*) AS count FROM projection_batches")
          .one().count;
        if (pending !== 0) {
          throw new ProjectionStoreError("hidden policy cannot change while a push is pending", 409);
        }
        if (meta.current_policy_epoch >= Number.MAX_SAFE_INTEGER) {
          throw new Error("hidden policy epoch exceeds the safe integer range");
        }
        const nextEpoch = meta.current_policy_epoch + 1;
        const count = this.sql
          .exec<{ count: number }>(
            `SELECT count(*) AS count FROM hidden_policy_paths
             WHERE epoch = ? AND path != ?`,
            meta.current_policy_epoch,
            request.path,
          )
          .one().count;
        if (count + (request.hidden ? 1 : 0) > MAX_HIDDEN_PATHS) {
          throw new ProjectionStoreError(
            `hidden policy exceeds the ${MAX_HIDDEN_PATHS}-path limit`,
            409,
          );
        }
        const retainedBytes = this.sql
          .exec<{ path: string }>(
            "SELECT path FROM hidden_policy_paths WHERE epoch = ? AND path != ?",
            meta.current_policy_epoch,
            request.path,
          )
          .toArray()
          .reduce((total, row) => total + new TextEncoder().encode(row.path).byteLength, 0);
        const policyBytes =
          retainedBytes + (request.hidden ? new TextEncoder().encode(request.path).byteLength : 0);
        if (policyBytes > MAX_HIDDEN_POLICY_BYTES) {
          throw new ProjectionStoreError(
            `hidden policy exceeds the ${MAX_HIDDEN_POLICY_BYTES}-byte limit`,
            409,
          );
        }
        this.sql.exec(
          "INSERT INTO hidden_policy_versions VALUES (?, ?)",
          nextEpoch,
          Date.now(),
        );
        this.sql.exec(
          `INSERT INTO hidden_policy_paths (epoch, path)
           SELECT ?, path FROM hidden_policy_paths WHERE epoch = ? AND path != ?`,
          nextEpoch,
          meta.current_policy_epoch,
          request.path,
        );
        if (request.hidden) {
          this.sql.exec(
            "INSERT INTO hidden_policy_paths VALUES (?, ?)",
            nextEpoch,
            request.path,
          );
        }
        this.sql.exec(
          "UPDATE projection_meta SET current_policy_epoch = ? WHERE singleton = 1",
          nextEpoch,
        );
        return { ok: true as const, changed: true, policyEpoch: nextEpoch };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  begin(value: unknown) {
    let request: BeginProjectionBatchRequest;
    try {
      request = decodeBeginProjectionBatch(value);
    } catch (error) {
      return failure(error, 400);
    }
    const incarnation = exactBuffer(request.incarnation);
    const batchId = exactBuffer(request.batchId);
    const requestHash = exactBuffer(
      this.kernel.hash([canonicalProjectionBatchBytes(request)]),
    );
    try {
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(incarnation);
        const result = this.batchResult(batchId);
        if (result !== undefined) {
          this.requireRequestHash(result.request_hash, requestHash);
          return {
            ok: true as const,
            pending: false,
            fence: result.final_fence,
            outcome: result.outcome,
          };
        }
        const previous = this.batch(batchId);
        if (previous !== undefined) {
          this.requireRequestHash(previous.request_hash, requestHash);
          return { ok: true as const, pending: true, fence: previous.fence };
        }
        const meta = this.meta();
        if (request.policyEpoch !== meta.current_policy_epoch) {
          throw new ProjectionStoreError(
            `projection policy epoch ${request.policyEpoch} is stale; current epoch is ${meta.current_policy_epoch}`,
            409,
          );
        }
        const pendingRefs = this.sql
          .exec<{ count: number }>("SELECT count(*) AS count FROM projection_batch_refs")
          .one().count;
        if (pendingRefs + request.updates.length > MAX_REPOSITORY_PROJECTION_REFS) {
          throw new ProjectionStoreError(
            `pending projection refs exceed the ${MAX_REPOSITORY_PROJECTION_REFS}-ref repository limit`,
            429,
          );
        }
        const pendingStates = this.sql
          .exec<{ count: number }>(
            "SELECT count(*) AS count FROM projection_states WHERE pending_batch_id IS NOT NULL",
          )
          .one().count;
        const requestedStates = request.updates.reduce(
          (count, update) => count + update.states.length,
          0,
        );
        if (pendingStates + requestedStates > MAX_PROJECTION_STATES) {
          throw new ProjectionStoreError(
            `pending projection states exceed the ${MAX_PROJECTION_STATES}-state repository limit`,
            429,
          );
        }
        const activeCursors = this.sql
          .exec<{ count: number }>("SELECT count(*) AS count FROM projection_cursors")
          .one().count;
        const pendingAdds = this.sql
          .exec<{ count: number }>(
            `SELECT count(*) AS count FROM projection_batch_refs
             WHERE expected_old_oid IS NULL AND proposed_state_id IS NOT NULL`,
          )
          .one().count;
        const requestedAdds = request.updates.filter(
          (update) => update.expectedOldOid === null && update.proposedState !== null,
        ).length;
        if (activeCursors + pendingAdds + requestedAdds > MAX_REPOSITORY_PROJECTION_REFS) {
          throw new ProjectionStoreError(
            `projection cursors exceed the ${MAX_REPOSITORY_PROJECTION_REFS}-ref repository limit`,
            429,
          );
        }
        for (const update of request.updates) {
          this.requireExpectedCursor(request.remote, update.bookmark, update.expectedOldOid);
          for (const state of update.states) this.requireDurableState(state);
        }
        const fence = this.nextFence(meta);
        this.sql.exec(
          "INSERT INTO projection_batches VALUES (?, ?, ?, ?, ?, ?, ?)",
          batchId,
          request.remote,
          request.policyEpoch,
          exactBuffer(request.machineId),
          fence,
          requestHash,
          Date.now(),
        );
        for (const [position, update] of request.updates.entries()) {
          const stateIds: number[] = [];
          for (const state of update.states) {
            this.storeReceipt(state.gitOid, state.publicCommitId);
            this.sql.exec(
              `INSERT INTO projection_states
               (remote, bookmark, git_oid, canonical_commit_id, public_commit_id,
                policy_epoch, pending_batch_id, activation_seq)
               VALUES (?, ?, ?, ?, ?, ?, ?, NULL)`,
              request.remote,
              update.bookmark,
              exactBuffer(state.gitOid),
              exactBuffer(state.canonicalCommitId),
              exactBuffer(state.publicCommitId),
              request.policyEpoch,
              batchId,
            );
            stateIds.push(
              this.sql.exec<{ id: number }>("SELECT last_insert_rowid() AS id").one()
                .id,
            );
          }
          const proposedStateId =
            update.proposedState === null ? null : stateIds[update.proposedState];
          try {
            this.sql.exec(
              "INSERT INTO projection_batch_refs VALUES (?, ?, ?, ?, ?, ?)",
              batchId,
              position,
              request.remote,
              update.bookmark,
              update.expectedOldOid === null ? null : exactBuffer(update.expectedOldOid),
              proposedStateId,
            );
          } catch (error) {
            if (error instanceof Error && error.message.includes("UNIQUE constraint failed")) {
              throw new ProjectionStoreError(
                `another push already owns ${request.remote}/${update.bookmark}`,
                409,
              );
            }
            throw error;
          }
        }
        return { ok: true as const, pending: true, fence };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  claim(batchIdValue: unknown, value: unknown) {
    let batchId: ArrayBuffer;
    let request: ReturnType<typeof decodeClaimProjectionBatch>;
    try {
      batchId = exactBuffer(decodeBatchId(batchIdValue));
      request = decodeClaimProjectionBatch(value);
    } catch (error) {
      return failure(error, 400);
    }
    try {
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(exactBuffer(request.incarnation));
        const batch = this.requireBatch(batchId);
        const fence = this.nextFence(this.meta());
        this.sql.exec(
          "UPDATE projection_batches SET owner_machine = ?, fence = ? WHERE batch_id = ?",
          exactBuffer(request.machineId),
          fence,
          batchId,
        );
        this.sql.exec(
          "INSERT OR IGNORE INTO projection_recovery_claims VALUES (?)",
          batchId,
        );
        return { ok: true as const, fence, previousFence: batch.fence };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  confirm(batchIdValue: unknown, value: unknown) {
    return this.withFence(batchIdValue, value, decodeProjectionFence, (batchId, request) => {
      const replay = this.replayFinished(batchId, request);
      if (replay !== undefined) return replay;
      this.requireFence(batchId, request);
      if (this.isRecoveryClaimed(batchId)) {
        throw new ProjectionStoreError(
          "claimed projection batch requires observed remote state recovery",
          409,
        );
      }
      return this.finish(batchId, request.fence, "accepted");
    });
  }

  recover(batchIdValue: unknown, value: unknown) {
    return this.withFence(batchIdValue, value, decodeRecoverProjectionBatch, (batchId, request) => {
      const replay = this.replayFinished(batchId, request);
      if (replay !== undefined) return replay;
      this.requireFence(batchId, request);
      const refs = this.batchRefs(batchId);
      this.requireObservationSet(refs, request.observations);
      let allProposed = true;
      let allExpected = true;
      for (const [index, ref] of refs.entries()) {
        const observed = request.observations[index].liveOid;
        const proposed = ref.proposed_git_oid === null ? null : new Uint8Array(ref.proposed_git_oid);
        const expected = ref.expected_old_oid === null ? null : new Uint8Array(ref.expected_old_oid);
        allProposed &&= compareNullableBytes(observed, proposed) === 0;
        allExpected &&= compareNullableBytes(observed, expected) === 0;
      }
      if (allProposed) return this.finish(batchId, request.fence, "accepted");
      if (allExpected && this.isRecoveryClaimed(batchId)) {
        throw new ProjectionStoreError(
          "claimed projection batch still matches its expected refs; replay the exact push before recovery",
          409,
        );
      }
      if (allExpected) {
        return this.finish(batchId, request.fence, "aborted");
      }
      throw new ProjectionStoreError(
        "remote refs are mixed or ambiguous; projection batch remains quarantined",
        409,
      );
    });
  }

  private isRecoveryClaimed(batchId: ArrayBuffer) {
    return (
      this.sql
        .exec<{ count: number }>(
          "SELECT count(*) AS count FROM projection_recovery_claims WHERE batch_id = ?",
          batchId,
        )
        .one().count !== 0
    );
  }

  private withFence<T extends ProjectionFenceRequest>(
    batchIdValue: unknown,
    value: unknown,
    decode: (value: unknown) => T,
    operation: (batchId: ArrayBuffer, request: T) => unknown,
  ) {
    let batchId: ArrayBuffer;
    let request: T;
    try {
      batchId = exactBuffer(decodeBatchId(batchIdValue));
      request = decode(value);
    } catch (error) {
      return failure(error, 400);
    }
    try {
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(exactBuffer(request.incarnation));
        return operation(batchId, request);
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  private finish(batchId: ArrayBuffer, fence: number, outcome: "accepted" | "aborted") {
    const batch = this.requireBatch(batchId);
    if (outcome === "accepted") {
      let activation = this.meta().activation_cursor;
      const drafts = this.sql
        .exec<{ state_id: number }>(
          "SELECT state_id FROM projection_states WHERE pending_batch_id = ? ORDER BY state_id",
          batchId,
        )
        .toArray();
      for (const draft of drafts) {
        if (activation >= Number.MAX_SAFE_INTEGER) {
          throw new Error("projection activation cursor exceeds the safe integer range");
        }
        activation += 1;
        this.sql.exec(
          `UPDATE projection_states
           SET pending_batch_id = NULL, activation_seq = ? WHERE state_id = ?`,
          activation,
          draft.state_id,
        );
      }
      for (const ref of this.batchRefs(batchId)) {
        if (ref.proposed_state_id === null) {
          this.sql.exec(
            "DELETE FROM projection_cursors WHERE remote = ? AND bookmark = ?",
            batch.remote,
            ref.bookmark,
          );
          continue;
        }
        this.sql.exec(
          `INSERT INTO projection_cursors VALUES (?, ?, ?)
           ON CONFLICT (remote, bookmark) DO UPDATE SET state_id = excluded.state_id`,
          batch.remote,
          ref.bookmark,
          ref.proposed_state_id,
        );
      }
      this.sql.exec(
        "UPDATE projection_meta SET activation_cursor = ? WHERE singleton = 1",
        activation,
      );
    } else {
      this.sql.exec(
        "DELETE FROM projection_states WHERE pending_batch_id = ?",
        batchId,
      );
    }
    this.sql.exec("DELETE FROM projection_batch_refs WHERE batch_id = ?", batchId);
    this.sql.exec("DELETE FROM projection_recovery_claims WHERE batch_id = ?", batchId);
    this.sql.exec("DELETE FROM projection_batches WHERE batch_id = ?", batchId);
    this.sql.exec(
      "INSERT INTO projection_batch_results VALUES (?, ?, ?, ?, ?)",
      batchId,
      batch.request_hash,
      fence,
      outcome,
      Date.now(),
    );
    return { ok: true as const, pending: false, fence, outcome };
  }

  private replayFinished(batchId: ArrayBuffer, request: ProjectionFenceRequest) {
    const result = this.batchResult(batchId);
    if (result === undefined) return undefined;
    if (result.final_fence !== request.fence) {
      throw new ProjectionStoreError("projection fencing token is stale", 409);
    }
    return {
      ok: true as const,
      pending: false,
      fence: result.final_fence,
      outcome: result.outcome,
    };
  }

  private requireFence(batchId: ArrayBuffer, request: ProjectionFenceRequest) {
    const batch = this.requireBatch(batchId);
    if (
      batch.fence !== request.fence ||
      !equalBytes(new Uint8Array(batch.owner_machine), request.machineId)
    ) {
      throw new ProjectionStoreError("projection owner or fencing token is stale", 409);
    }
  }

  private requireObservationSet(refs: BatchRefRow[], observations: ProjectionObservation[]) {
    if (
      refs.length !== observations.length ||
      refs.some((ref, index) => ref.bookmark !== observations[index].bookmark)
    ) {
      throw new ProjectionStoreError("observations must cover the exact pending ref set", 409);
    }
  }

  private requireDurableState(state: {
    canonicalCommitId: Uint8Array;
    publicCommitId: Uint8Array;
  }) {
    for (const [label, id] of [
      ["canonical", state.canonicalCommitId],
      ["public", state.publicCommitId],
    ] as const) {
      if (id.every((byte) => byte === 0)) {
        throw new ProjectionStoreError(`${label} commit ${toHex(id)} is not cloud durable`, 409);
      }
      const commitId = exactBuffer(id);
      const missing = this.findMissingReachableObject(commitId);
      if (missing !== undefined) {
        const missingId = toHex(new Uint8Array(missing.id));
        if (missing.kind === KIND.commit && missingId === toHex(id)) {
          throw new ProjectionStoreError(
            `${label} commit ${toHex(id)} is not cloud durable`,
            409,
          );
        }
        throw new ProjectionStoreError(
          `${label} commit ${toHex(id)} closure is missing ${KIND_BY_NUMBER[missing.kind] ?? `kind ${missing.kind}`} ${missingId}`,
          409,
        );
      }
      this.markClosureComplete(commitId);
    }
  }

  private findMissingReachableObject(commitId: ArrayBuffer): MissingObjectRow | undefined {
    return this.sql
      .exec<MissingObjectRow>(
        `WITH RECURSIVE reachable(kind, id) AS (
           VALUES (${KIND.commit}, ?)
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
           AND NOT (reachable.kind = ${KIND.commit} AND reachable.id = zeroblob(64))
         ORDER BY reachable.kind, reachable.id
         LIMIT 1`,
        commitId,
      )
      .toArray()[0];
  }

  private markClosureComplete(commitId: ArrayBuffer) {
    this.sql.exec(
      `INSERT OR IGNORE INTO complete_object_closures
       WITH RECURSIVE reachable(kind, id) AS (
         VALUES (${KIND.commit}, ?)
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
      commitId,
    );
  }

  private storeReceipt(gitOid: Uint8Array, publicCommitId: Uint8Array) {
    const oid = exactBuffer(gitOid);
    const existing = this.sql
      .exec<ReceiptRow>("SELECT public_commit_id FROM git_receipts WHERE git_oid = ?", oid)
      .toArray()[0];
    if (existing !== undefined) {
      if (!equalBytes(new Uint8Array(existing.public_commit_id), publicCommitId)) {
        throw new ProjectionStoreError(
          `Git object ${toHex(gitOid)} already has a different immutable receipt`,
          409,
        );
      }
      return;
    }
    this.sql.exec(
      "INSERT INTO git_receipts VALUES (?, ?)",
      oid,
      exactBuffer(publicCommitId),
    );
  }

  private requireExpectedCursor(
    remote: string,
    bookmark: string,
    expected: Uint8Array | null,
  ) {
    const row = this.sql
      .exec<{ git_oid: ArrayBuffer }>(
        `SELECT states.git_oid FROM projection_cursors AS cursors
         JOIN projection_states AS states ON states.state_id = cursors.state_id
         WHERE cursors.remote = ? AND cursors.bookmark = ?`,
        remote,
        bookmark,
      )
      .toArray()[0];
    const actual = row === undefined ? null : new Uint8Array(row.git_oid);
    if (compareNullableBytes(actual, expected) !== 0) {
      throw new ProjectionStoreError(
        `projection cursor for ${remote}/${bookmark} does not match expected old Git ID`,
        409,
      );
    }
  }

  private batchRefs(batchId: ArrayBuffer): BatchRefRow[] {
    return this.sql
      .exec<BatchRefRow>(
        `SELECT refs.bookmark, refs.expected_old_oid, refs.proposed_state_id,
                states.git_oid AS proposed_git_oid
         FROM projection_batch_refs AS refs
         LEFT JOIN projection_states AS states ON states.state_id = refs.proposed_state_id
         WHERE refs.batch_id = ? ORDER BY refs.position`,
        batchId,
      )
      .toArray();
  }

  private requireBatch(batchId: ArrayBuffer): BatchRow {
    const batch = this.batch(batchId);
    if (batch === undefined) throw new ProjectionStoreError("projection batch does not exist", 404);
    return batch;
  }

  private batch(batchId: ArrayBuffer): BatchRow | undefined {
    return this.sql
      .exec<BatchRow>(
        `SELECT remote, policy_epoch, owner_machine, fence, request_hash
         FROM projection_batches WHERE batch_id = ?`,
        batchId,
      )
      .toArray()[0];
  }

  private batchResult(batchId: ArrayBuffer): BatchResultRow | undefined {
    return this.sql
      .exec<BatchResultRow>(
        `SELECT request_hash, final_fence, outcome
         FROM projection_batch_results WHERE batch_id = ?`,
        batchId,
      )
      .toArray()[0];
  }

  private requireRequestHash(actual: ArrayBuffer, expected: ArrayBuffer) {
    if (!equalBytes(new Uint8Array(actual), new Uint8Array(expected))) {
      throw new ProjectionStoreError(
        "projection batch ID was already used for a different request",
        409,
      );
    }
  }

  private requireIncarnation(incarnation: ArrayBuffer) {
    const state = this.sql
      .exec<RepositoryStateRow>("SELECT incarnation FROM repository_state WHERE singleton = 1")
      .toArray()[0];
    if (state === undefined) throw new ProjectionStoreError("repository is not initialized", 409);
    if (!equalBytes(new Uint8Array(state.incarnation), new Uint8Array(incarnation))) {
      throw new ProjectionStoreError("repository incarnation does not match", 409);
    }
  }

  private nextFence(meta: ProjectionMetaRow): number {
    if (meta.next_fence >= Number.MAX_SAFE_INTEGER) {
      throw new Error("projection fencing token exceeds the safe integer range");
    }
    const fence = meta.next_fence + 1;
    this.sql.exec(
      "UPDATE projection_meta SET next_fence = ? WHERE singleton = 1",
      fence,
    );
    return fence;
  }

  private meta(): ProjectionMetaRow {
    return this.sql
      .exec<ProjectionMetaRow>(
        `SELECT current_policy_epoch, next_fence, activation_cursor
         FROM projection_meta WHERE singleton = 1`,
      )
      .one();
  }

  private handleExpected(error: unknown) {
    if (error instanceof WebAssembly.RuntimeError) this.kernel.reset();
    if (error instanceof ProjectionStoreError) return failure(error, error.status);
    throw error;
  }
}

function decodeIncarnationOnly(value: unknown): Uint8Array {
  return decodeClaimProjectionBatch({ incarnation: value, machineId: "00".repeat(16) }).incarnation;
}

function decodeBatchId(value: unknown): Uint8Array {
  return decodeClaimProjectionBatch({ incarnation: value, machineId: "00".repeat(16) }).incarnation;
}

function failure(error: unknown, status: number) {
  return {
    ok: false as const,
    status,
    error: error instanceof Error ? error.message : "projection request failed",
  };
}
