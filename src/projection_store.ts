import { KIND, KIND_BY_NUMBER, Kernel, equalBytes, exactBuffer, toHex } from "./kernel";
import { findMissingReachableObject, markClosureComplete } from "./closure_authority";
import {
  BeginProjectionBatchRequest,
  MAX_PROJECTION_STATES,
  MAX_REPOSITORY_PROJECTION_REFS,
  ProjectionProtocolError,
  ProjectionFenceRequest,
  ProjectionObservation,
  RecordFetchRequest,
  canonicalFetchBytes,
  canonicalProjectionBatchBytes,
  compareNullableBytes,
  decodeBeginProjectionBatch,
  decodeClaimProjectionBatch,
  decodeProjectionShortId,
  decodeRecordFetch,
  decodeRecoverProjectionBatch,
} from "./projection_protocol";
import {
  RemoteProtocolError,
  decodeRemoteIncarnation,
  decodeRemoteName,
  decodeSetRemote,
} from "./remote_protocol";

interface RepositoryStateRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
}

interface ProjectionMetaRow extends Record<string, SqlStorageValue> {
  next_fence: number;
  activation_cursor: number;
}

interface BatchRow extends Record<string, SqlStorageValue> {
  remote: string;
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
  hidden_set_id: ArrayBuffer | null;
  activation_seq: number;
}

interface PendingRow extends Record<string, SqlStorageValue> {
  batch_id: ArrayBuffer;
  remote: string;
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
  hidden_set_id: ArrayBuffer | null;
}

interface MappingRow extends Record<string, SqlStorageValue> {
  remote: string;
  bookmark: string;
  git_oid: ArrayBuffer;
  canonical_commit_id: ArrayBuffer;
  public_commit_id: ArrayBuffer;
  hidden_set_id: ArrayBuffer | null;
  activation_seq: number;
}

interface ReceiptRow extends Record<string, SqlStorageValue> {
  public_commit_id: ArrayBuffer;
}

interface RemoteRow extends Record<string, SqlStorageValue> {
  name: string;
  url: string;
}

interface FetchResultRow extends Record<string, SqlStorageValue> {
  request_hash: ArrayBuffer;
  activation_cursor: number;
}

interface ActiveStateRow extends Record<string, SqlStorageValue> {
  state_id: number;
  canonical_commit_id: ArrayBuffer;
  hidden_set_id: ArrayBuffer | null;
}

interface ActiveLineageRow extends Record<string, SqlStorageValue> {
  git_oid: ArrayBuffer;
  canonical_commit_id: ArrayBuffer;
  hidden_set_id: ArrayBuffer | null;
}

const REPLAY_RETENTION_MS = 7 * 24 * 60 * 60 * 1_000;
const MAX_REPLAY_RESULTS = 65_536;
const PRUNE_BATCH = 256;

class ProjectionStoreError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string = defaultProjectionErrorCode(status),
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

  setRemote(nameValue: unknown, value: unknown) {
    let name: string;
    let request: ReturnType<typeof decodeSetRemote>;
    try {
      name = decodeRemoteName(nameValue);
      request = decodeSetRemote(value);
    } catch (error) {
      return remoteRequestFailure(error);
    }
    try {
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(exactBuffer(request.incarnation));
        const existing = this.sql
          .exec<RemoteRow>("SELECT name, url FROM remotes WHERE name = ?", name)
          .toArray()[0];
        if (existing?.url === request.url) {
          return { ok: true as const, remote: { name, url: request.url } };
        }
        if (existing !== undefined) this.clearRemoteJournal(name);
        this.sql.exec(
          `INSERT INTO remotes VALUES (?, ?)
           ON CONFLICT (name) DO UPDATE SET url = excluded.url`,
          name,
          request.url,
        );
        return { ok: true as const, remote: { name, url: request.url } };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  listRemotes(incarnationValue: unknown) {
    let incarnation: Uint8Array;
    try {
      incarnation = decodeRemoteIncarnation(incarnationValue);
    } catch (error) {
      return remoteRequestFailure(error);
    }
    try {
      this.requireIncarnation(exactBuffer(incarnation));
      return {
        ok: true as const,
        remotes: this.sql
          .exec<RemoteRow>("SELECT name, url FROM remotes ORDER BY name")
          .toArray()
          .map(({ name, url }) => ({ name, url })),
      };
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  get(incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    let incarnation: ArrayBuffer;
    try {
      incarnation = exactBuffer(decodeProjectionShortId(incarnationValue, "incarnation"));
      if (
        typeof afterValue !== "number" ||
        !Number.isSafeInteger(afterValue) ||
        afterValue < 0
      ) {
        throw new ProjectionStoreError(
          "projection cursor must be a non-negative integer",
          400,
          "invalid-projection-cursor",
        );
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
          "invalid-projection-high-water",
        );
      }
      const cursors = this.sql
        .exec<CursorRow>(
          `SELECT cursors.remote, cursors.bookmark, states.git_oid,
                  states.canonical_commit_id, states.public_commit_id,
                  states.hidden_set_id, states.activation_seq
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
          hiddenSetId:
            row.hidden_set_id === null ? null : toHex(new Uint8Array(row.hidden_set_id)),
          activationSequence: row.activation_seq,
        }));
      const pending = this.sql
        .exec<PendingRow>(
          `SELECT batch_id, remote, owner_machine, fence
           FROM projection_batches ORDER BY batch_id`,
        )
        .toArray()
        .map((row) => ({
          batchId: toHex(new Uint8Array(row.batch_id)),
          remote: row.remote,
          ownerMachine: toHex(new Uint8Array(row.owner_machine)),
          fence: row.fence,
          refs: this.pendingRefSnapshot(row.batch_id),
        }));
      const mappingRows = this.sql
        .exec<MappingRow>(
          `SELECT remote, bookmark, git_oid, canonical_commit_id,
                  public_commit_id, hidden_set_id, activation_seq
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
          hiddenSetId:
            row.hidden_set_id === null ? null : toHex(new Uint8Array(row.hidden_set_id)),
        }));
      const nextAfter =
        pageRows.length === 0
          ? afterValue
          : pageRows[pageRows.length - 1].activation_seq;
      return {
        ok: true as const,
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
      batchId = exactBuffer(decodeProjectionShortId(batchIdValue, "batchId"));
      incarnation = exactBuffer(decodeProjectionShortId(incarnationValue, "incarnation"));
    } catch (error) {
      return failure(error, 400);
    }
    try {
      this.requireIncarnation(incarnation);
      const batch = this.requireBatch(batchId);
      const updates = this.batchRefs(batchId).map((ref) => {
        const states = this.sql
          .exec<ReplayStateRow>(
            `SELECT state_id, git_oid, canonical_commit_id, public_commit_id, hidden_set_id
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
            hiddenSetId:
              state.hidden_set_id === null
                ? null
                : toHex(new Uint8Array(state.hidden_set_id)),
          })),
          proposedState,
        };
      });
      return {
        ok: true as const,
        batchId: toHex(new Uint8Array(batchId)),
        remote: batch.remote,
        ownerMachine: toHex(new Uint8Array(batch.owner_machine)),
        fence: batch.fence,
        updates,
      };
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  begin(value: unknown, authenticatedMachineId: string) {
    let request: BeginProjectionBatchRequest;
    try {
      request = decodeBeginProjectionBatch(value);
      requireAuthenticatedMachine(request.machineId, authenticatedMachineId);
    } catch (error) {
      return requestFailure(error, "invalid-projection-request");
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
        const pendingRefs = this.sql
          .exec<{ count: number }>("SELECT count(*) AS count FROM projection_batch_refs")
          .one().count;
        if (pendingRefs + request.updates.length > MAX_REPOSITORY_PROJECTION_REFS) {
          throw new ProjectionStoreError(
            `pending projection refs exceed the ${MAX_REPOSITORY_PROJECTION_REFS}-ref repository limit`,
            429,
            "projection-pending-ref-limit",
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
            "projection-pending-state-limit",
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
            "projection-ref-limit",
          );
        }
        this.requireExpectedCursors(
          request.remote,
          request.updates.map((update) => ({
            bookmark: update.bookmark,
            expected: update.expectedOldOid,
          })),
        );
        for (const update of request.updates) {
          for (const state of update.states) this.requireDurableState(state);
        }
        const fence = this.nextFence(meta);
        this.sql.exec(
          "INSERT INTO projection_batches VALUES (?, ?, ?, ?, ?, ?)",
          batchId,
          request.remote,
          exactBuffer(request.machineId),
          fence,
          requestHash,
          Date.now(),
        );
        for (const [position, update] of request.updates.entries()) {
          const stateIds: number[] = [];
          for (const state of update.states) {
            this.storeReceipt(state.gitOid, state.publicCommitId);
            const inserted = this.sql.exec<{ state_id: number }>(
              `INSERT INTO projection_states
               (remote, bookmark, git_oid, canonical_commit_id, public_commit_id,
                hidden_set_id, pending_batch_id, activation_seq)
               VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
               RETURNING state_id`,
              request.remote,
              update.bookmark,
              exactBuffer(state.gitOid),
              exactBuffer(state.canonicalCommitId),
              exactBuffer(state.publicCommitId),
              state.hiddenSetId === null ? null : exactBuffer(state.hiddenSetId),
              batchId,
            ).one();
            stateIds.push(inserted.state_id);
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
                "push-in-progress",
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

  recordFetch(value: unknown, authenticatedMachineId: string) {
    let request: RecordFetchRequest;
    try {
      request = decodeRecordFetch(value);
      requireAuthenticatedMachine(
        request.machineId,
        authenticatedMachineId,
        "fetch-machine-mismatch",
      );
    } catch (error) {
      return requestFailure(error, "invalid-fetch-request");
    }
    const incarnation = exactBuffer(request.incarnation);
    const fetchId = exactBuffer(request.fetchId);
    const requestHash = exactBuffer(this.kernel.hash([canonicalFetchBytes(request)]));
    const nowMs = Date.now();
    try {
      this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(incarnation, "repository-incarnation-mismatch");
        this.pruneExpiredFetchResults(nowMs - REPLAY_RETENTION_MS);
      });
      return this.ctx.storage.transactionSync(() => {
        // 1. A completed fetch is the idempotency record and response payload.
        const recorded = this.fetchResult(fetchId);
        if (recorded !== undefined) {
          if (!equalBytes(new Uint8Array(recorded.request_hash), new Uint8Array(requestHash))) {
            throw new ProjectionStoreError(
              "fetch ID was already used for a different request",
              409,
              "fetch-request-mismatch",
            );
          }
          return {
            ok: true as const,
            fetchId: toHex(request.fetchId),
            activationCursor: recorded.activation_cursor,
          };
        }

        this.requireIncarnation(incarnation, "repository-incarnation-mismatch");
        if (!this.remoteExists(request.remote)) {
          throw new ProjectionStoreError(
            `remote ${request.remote} is not registered`,
            404,
            "remote-not-found",
          );
        }
        this.requireReplayCapacity("fetch");
        if (request.refs.length !== 0) {
          // 2. Fetch observes the exact last accepted state for every ref.
          this.requireExpectedCursors(
            request.remote,
            request.refs.map((ref) => ({
              bookmark: ref.bookmark,
              expected: ref.expectedCursorOid,
            })),
            "fetch-cursor-stale",
          );

          // 3. Push recovery owns any ref it has quarantined.
          const pendingBookmarks = this.pendingPushBookmarks(
            request.remote,
            request.refs.map((ref) => ref.bookmark),
          );
          for (const ref of request.refs) {
            if (pendingBookmarks.has(ref.bookmark)) {
              throw new ProjectionStoreError(
                `fetch overlaps a pending push for ${request.remote}/${ref.bookmark}`,
                409,
                "fetch-overlaps-pending-push",
              );
            }
          }
        }

        // 4. Both the raw public and lifted private histories must be durable.
        for (const ref of request.refs) {
          for (const state of ref.states) {
            this.requireDurableState(state, "fetch-commit-not-durable");
          }
        }
        for (const receipt of request.receipts) {
          this.requireDurableCommit(
            "public",
            receipt.publicCommitId,
            "fetch-commit-not-durable",
          );
        }

        // 5. Receipts are immutable, including duplicate entries in this request.
        const receipts = this.requireFetchReceiptConsistency(request);

        // 6. Every state must agree with its immutable Git-to-public receipt.
        for (const ref of request.refs) {
          for (const state of ref.states) {
            const publicCommitId = receipts.get(toHex(state.gitOid));
            if (
              publicCommitId === undefined ||
              !equalBytes(publicCommitId, state.publicCommitId)
            ) {
              throw new ProjectionStoreError(
                `fetch state for Git object ${toHex(state.gitOid)} does not match an immutable receipt`,
                409,
                "fetch-state-receipt-mismatch",
              );
            }
          }
        }

        if (request.refs.length !== 0) {
          // 7. One Git object has one private lineage across current bookmark tips.
          this.requireUnambiguousFetchLineage(request);
          this.requireFetchCursorCapacity(request);
        }

        const existingStateIds = new Map<string, number>();
        for (const ref of request.refs) {
          if (ref.proposedState !== null) continue;
          const existing = this.activeStateForObserved(
            request.remote,
            ref.bookmark,
            ref.observedGitOid,
          );
          if (existing === undefined) {
            throw new ProjectionStoreError(
              `observed Git object ${toHex(ref.observedGitOid)} has no active state for ${request.remote}/${ref.bookmark}`,
              409,
              "fetch-observed-state-not-found",
            );
          }
          existingStateIds.set(ref.bookmark, existing.state_id);
        }

        for (const receipt of request.receipts) {
          this.storeReceipt(receipt.gitOid, receipt.publicCommitId);
        }

        let activation = this.meta().activation_cursor;
        for (const ref of request.refs) {
          const stateIds: number[] = [];
          for (const state of ref.states) {
            if (activation >= Number.MAX_SAFE_INTEGER) {
              throw new Error("projection activation cursor exceeds the safe integer range");
            }
            activation += 1;
            const inserted = this.sql.exec<{ state_id: number }>(
              `INSERT INTO projection_states
               (remote, bookmark, git_oid, canonical_commit_id, public_commit_id,
                hidden_set_id, pending_batch_id, activation_seq)
               VALUES (?, ?, ?, ?, ?, ?, NULL, ?)
               RETURNING state_id`,
              request.remote,
              ref.bookmark,
              exactBuffer(state.gitOid),
              exactBuffer(state.canonicalCommitId),
              exactBuffer(state.publicCommitId),
              state.hiddenSetId === null ? null : exactBuffer(state.hiddenSetId),
              activation,
            ).one();
            stateIds.push(inserted.state_id);
          }
          const proposedStateId =
            ref.proposedState === null
              ? existingStateIds.get(ref.bookmark)
              : stateIds[ref.proposedState];
          if (proposedStateId === undefined) throw new Error("fetch cursor state is missing");
          this.sql.exec(
            `INSERT INTO projection_cursors VALUES (?, ?, ?)
             ON CONFLICT (remote, bookmark) DO UPDATE SET state_id = excluded.state_id`,
            request.remote,
            ref.bookmark,
            proposedStateId,
          );
        }
        if (request.refs.length !== 0) {
          this.sql.exec(
            "UPDATE projection_meta SET activation_cursor = ? WHERE singleton = 1",
            activation,
          );
        }
        this.sql.exec(
          `INSERT INTO projection_fetch_results
           (fetch_id, remote, request_hash, activation_cursor, created_at_ms)
           VALUES (?, ?, ?, ?, ?)`,
          fetchId,
          request.remote,
          requestHash,
          activation,
          nowMs,
        );
        return {
          ok: true as const,
          fetchId: toHex(request.fetchId),
          activationCursor: activation,
        };
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  claim(batchIdValue: unknown, value: unknown, authenticatedMachineId: string) {
    let batchId: ArrayBuffer;
    let request: ReturnType<typeof decodeClaimProjectionBatch>;
    try {
      batchId = exactBuffer(decodeProjectionShortId(batchIdValue, "batchId"));
      request = decodeClaimProjectionBatch(value);
      requireAuthenticatedMachine(request.machineId, authenticatedMachineId);
    } catch (error) {
      return requestFailure(error, "invalid-projection-request");
    }
    try {
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(exactBuffer(request.incarnation));
        const result = this.batchResult(batchId);
        if (result !== undefined) {
          return {
            ok: true as const,
            pending: false,
            fence: result.final_fence,
            outcome: result.outcome,
          };
        }
        // Once retention removes a finished result, claim follows its normal
        // non-replay path and returns projection-batch-not-found.
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

  recover(batchIdValue: unknown, value: unknown, authenticatedMachineId: string) {
    return this.withFence(
      batchIdValue,
      value,
      decodeRecoverProjectionBatch,
      authenticatedMachineId,
      (batchId, request, nowMs) => {
        const replay = this.replayFinished(batchId, request);
        if (replay !== undefined) return replay;
        this.requireFence(batchId, request);
        const refs = this.batchRefs(batchId);
        this.requireObservationSet(refs, request.observations);
        let allProposed = true;
        let allExpected = true;
        for (const [index, ref] of refs.entries()) {
          const observed = request.observations[index].liveOid;
          const proposed =
            ref.proposed_git_oid === null
              ? null
              : new Uint8Array(ref.proposed_git_oid);
          const expected =
            ref.expected_old_oid === null
              ? null
              : new Uint8Array(ref.expected_old_oid);
          allProposed &&= compareNullableBytes(observed, proposed) === 0;
          allExpected &&= compareNullableBytes(observed, expected) === 0;
        }
        if (allProposed) {
          return this.finish(batchId, request.fence, "accepted", nowMs);
        }
        if (allExpected && this.isRecoveryClaimed(batchId)) {
          throw new ProjectionStoreError(
            "claimed projection batch still matches its expected refs; replay the exact push before recovery",
            409,
            "projection-replay-required",
          );
        }
        if (allExpected) {
          return this.finish(batchId, request.fence, "aborted", nowMs);
        }
        throw new ProjectionStoreError(
          "remote refs are mixed or ambiguous; projection batch remains quarantined",
          409,
          "projection-remote-state-ambiguous",
        );
      },
    );
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
    authenticatedMachineId: string,
    operation: (batchId: ArrayBuffer, request: T, nowMs: number) => unknown,
  ) {
    let batchId: ArrayBuffer;
    let request: T;
    try {
      batchId = exactBuffer(decodeProjectionShortId(batchIdValue, "batchId"));
      request = decode(value);
      requireAuthenticatedMachine(request.machineId, authenticatedMachineId);
    } catch (error) {
      return requestFailure(error, "invalid-projection-request");
    }
    const nowMs = Date.now();
    try {
      this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(exactBuffer(request.incarnation));
        this.pruneExpiredBatchResults(nowMs - REPLAY_RETENTION_MS);
      });
      return this.ctx.storage.transactionSync(() => {
        this.requireIncarnation(exactBuffer(request.incarnation));
        return operation(batchId, request, nowMs);
      });
    } catch (error) {
      return this.handleExpected(error);
    }
  }

  private finish(
    batchId: ArrayBuffer,
    fence: number,
    outcome: "accepted" | "aborted",
    nowMs: number,
  ) {
    const batch = this.requireBatch(batchId);
    this.requireReplayCapacity("batch");
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
      `INSERT INTO projection_batch_results
       (batch_id, remote, request_hash, final_fence, outcome, finished_at_ms)
       VALUES (?, ?, ?, ?, ?, ?)`,
      batchId,
      batch.remote,
      batch.request_hash,
      fence,
      outcome,
      nowMs,
    );
    return { ok: true as const, pending: false, fence, outcome };
  }

  private clearRemoteJournal(remote: string) {
    this.sql.exec(
      "DELETE FROM projection_recovery_claims WHERE batch_id IN (SELECT batch_id FROM projection_batches WHERE remote = ?)",
      remote,
    );
    this.sql.exec("DELETE FROM projection_batch_refs WHERE remote = ?", remote);
    this.sql.exec("DELETE FROM projection_cursors WHERE remote = ?", remote);
    this.sql.exec("DELETE FROM projection_states WHERE remote = ?", remote);
    this.sql.exec("DELETE FROM projection_batches WHERE remote = ?", remote);
    this.sql.exec("DELETE FROM projection_batch_results WHERE remote = ?", remote);
    this.sql.exec("DELETE FROM projection_fetch_results WHERE remote = ?", remote);
  }

  private replayFinished(batchId: ArrayBuffer, request: ProjectionFenceRequest) {
    const result = this.batchResult(batchId);
    if (result === undefined) return undefined;
    if (result.final_fence !== request.fence) {
      throw new ProjectionStoreError(
        "projection fencing token is stale",
        409,
        "projection-fence-stale",
      );
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
      throw new ProjectionStoreError(
        "projection owner or fencing token is stale",
        409,
        "projection-owner-stale",
      );
    }
  }

  private requireObservationSet(refs: BatchRefRow[], observations: ProjectionObservation[]) {
    if (
      refs.length !== observations.length ||
      refs.some((ref, index) => ref.bookmark !== observations[index].bookmark)
    ) {
      throw new ProjectionStoreError(
        "observations must cover the exact pending ref set",
        409,
        "projection-observation-set-mismatch",
      );
    }
  }

  private requireDurableState(state: {
    canonicalCommitId: Uint8Array;
    publicCommitId: Uint8Array;
  }, code?: string) {
    for (const [label, id] of [
      ["canonical", state.canonicalCommitId],
      ["public", state.publicCommitId],
    ] as const) {
      this.requireDurableCommit(label, id, code);
    }
  }

  private requireDurableCommit(label: string, id: Uint8Array, code?: string) {
    if (id.every((byte) => byte === 0)) {
      throw new ProjectionStoreError(
        `${label} commit ${toHex(id)} is not cloud durable`,
        409,
        code ?? "projection-commit-not-durable",
      );
    }
    const commitId = exactBuffer(id);
    const missing = findMissingReachableObject(this.sql, KIND.commit, commitId, [KIND.commit]);
    if (missing !== undefined) {
      const missingId = toHex(new Uint8Array(missing.id));
      if (missing.kind === KIND.commit && missingId === toHex(id)) {
        throw new ProjectionStoreError(
          `${label} commit ${toHex(id)} is not cloud durable`,
          409,
          code ?? "projection-commit-not-durable",
        );
      }
      throw new ProjectionStoreError(
        `${label} commit ${toHex(id)} closure is missing ${KIND_BY_NUMBER[missing.kind] ?? `kind ${missing.kind}`} ${missingId}`,
        409,
        code ?? "projection-commit-not-durable",
      );
    }
    markClosureComplete(this.sql, KIND.commit, commitId);
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
          "projection-receipt-mismatch",
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

  private requireExpectedCursors(
    remote: string,
    expectedCursors: Array<{ bookmark: string; expected: Uint8Array | null }>,
    code?: string,
  ) {
    const placeholders = expectedCursors.map(() => "?").join(", ");
    const rows = this.sql
      .exec<{ bookmark: string; git_oid: ArrayBuffer }>(
        `SELECT cursors.bookmark, states.git_oid
         FROM projection_cursors AS cursors
         JOIN projection_states AS states ON states.state_id = cursors.state_id
         WHERE cursors.remote = ? AND cursors.bookmark IN (${placeholders})`,
        remote,
        ...expectedCursors.map((cursor) => cursor.bookmark),
      )
      .toArray();
    const actualByBookmark = new Map(
      rows.map((row) => [row.bookmark, new Uint8Array(row.git_oid)]),
    );
    for (const { bookmark, expected } of expectedCursors) {
      const actual = actualByBookmark.get(bookmark) ?? null;
      if (compareNullableBytes(actual, expected) !== 0) {
        throw new ProjectionStoreError(
          `projection cursor for ${remote}/${bookmark} does not match expected old Git ID`,
          409,
          code ?? "projection-cursor-stale",
        );
      }
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
    if (batch === undefined) {
      throw new ProjectionStoreError(
        "projection batch does not exist",
        404,
        "projection-batch-not-found",
      );
    }
    return batch;
  }

  private batch(batchId: ArrayBuffer): BatchRow | undefined {
    return this.sql
      .exec<BatchRow>(
        `SELECT remote, owner_machine, fence, request_hash
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
        "projection-replay-mismatch",
      );
    }
  }

  private requireIncarnation(incarnation: ArrayBuffer, code?: string) {
    const state = this.sql
      .exec<RepositoryStateRow>("SELECT incarnation FROM repository_state WHERE singleton = 1")
      .toArray()[0];
    if (state === undefined) {
      throw new ProjectionStoreError(
        "repository is not initialized",
        409,
        code ?? "repository-not-initialized",
      );
    }
    if (!equalBytes(new Uint8Array(state.incarnation), new Uint8Array(incarnation))) {
      throw new ProjectionStoreError(
        "repository incarnation does not match",
        409,
        code ?? "repository-incarnation-mismatch",
      );
    }
  }

  private remoteExists(remote: string) {
    return (
      this.sql.exec<{ count: number }>("SELECT count(*) AS count FROM remotes WHERE name = ?", remote)
        .one().count !== 0
    );
  }

  private requireFetchCursorCapacity(request: RecordFetchRequest) {
    const cursorCount = this.sql
      .exec<{ count: number }>("SELECT count(*) AS count FROM projection_cursors")
      .one().count;
    const additions = request.refs.filter((ref) => ref.expectedCursorOid === null).length;
    if (cursorCount + additions > MAX_REPOSITORY_PROJECTION_REFS) {
      throw new ProjectionStoreError(
        `projection cursors exceed the ${MAX_REPOSITORY_PROJECTION_REFS}-ref repository limit`,
        429,
        "fetch-repository-ref-limit",
      );
    }
  }

  private pendingPushBookmarks(remote: string, bookmarks: string[]) {
    const placeholders = bookmarks.map(() => "?").join(", ");
    return new Set(
      this.sql
        .exec<{ bookmark: string }>(
          `SELECT bookmark FROM projection_batch_refs
           WHERE remote = ? AND bookmark IN (${placeholders})`,
          remote,
          ...bookmarks,
        )
        .toArray()
        .map((row) => row.bookmark),
    );
  }

  private pruneExpiredFetchResults(cutoffMs: number) {
    // A state-changing retry after expiry is evaluated as a new fetch and
    // normally returns fetch-cursor-stale because the original moved its cursor.
    this.sql.exec(
      `DELETE FROM projection_fetch_results
       WHERE fetch_id IN (
         SELECT fetch_id FROM projection_fetch_results
         WHERE created_at_ms < ?
         ORDER BY created_at_ms
         LIMIT ${PRUNE_BATCH}
       )`,
      cutoffMs,
    );
  }

  private pruneExpiredBatchResults(cutoffMs: number) {
    // Once a result expires, claim and recover take the non-replay path and
    // return projection-batch-not-found because no pending batch remains.
    this.sql.exec(
      `DELETE FROM projection_batch_results
       WHERE batch_id IN (
         SELECT batch_id FROM projection_batch_results
         WHERE finished_at_ms < ?
         ORDER BY finished_at_ms
         LIMIT ${PRUNE_BATCH}
       )`,
      cutoffMs,
    );
  }

  private requireReplayCapacity(kind: "fetch" | "batch") {
    const table =
      kind === "fetch" ? "projection_fetch_results" : "projection_batch_results";
    const count = this.sql
      .exec<{ count: number }>(`SELECT count(*) AS count FROM ${table}`)
      .one().count;
    if (count >= MAX_REPLAY_RESULTS) {
      throw new ProjectionStoreError(
        `projection ${kind} replay result quota is exhausted`,
        429,
        `projection-${kind}-result-limit`,
      );
    }
  }

  private requireFetchReceiptConsistency(request: RecordFetchRequest) {
    const receipts = new Map<string, Uint8Array>();
    for (const receipt of request.receipts) {
      const key = toHex(receipt.gitOid);
      const requested = receipts.get(key);
      if (requested !== undefined && !equalBytes(requested, receipt.publicCommitId)) {
        throw new ProjectionStoreError(
          `Git object ${key} has conflicting receipts in the fetch request`,
          409,
          "git-receipt-conflict",
        );
      }
      const existing = this.sql
        .exec<ReceiptRow>(
          "SELECT public_commit_id FROM git_receipts WHERE git_oid = ?",
          exactBuffer(receipt.gitOid),
        )
        .toArray()[0];
      if (
        existing !== undefined &&
        !equalBytes(new Uint8Array(existing.public_commit_id), receipt.publicCommitId)
      ) {
        throw new ProjectionStoreError(
          `Git object ${key} already has a different immutable receipt`,
          409,
          "git-receipt-conflict",
        );
      }
      receipts.set(key, receipt.publicCommitId);
    }
    for (const ref of request.refs) {
      for (const state of ref.states) {
        const key = toHex(state.gitOid);
        if (receipts.has(key)) continue;
        const existing = this.sql
          .exec<ReceiptRow>(
            "SELECT public_commit_id FROM git_receipts WHERE git_oid = ?",
            exactBuffer(state.gitOid),
          )
          .toArray()[0];
        if (existing !== undefined) {
          receipts.set(key, new Uint8Array(existing.public_commit_id));
        }
      }
    }
    return receipts;
  }

  private requireUnambiguousFetchLineage(request: RecordFetchRequest) {
    const requested = new Map<
      string,
      { canonicalCommitId: Uint8Array; hiddenSetId: Uint8Array | null }
    >();
    for (const ref of request.refs) {
      for (const state of ref.states) {
        const key = toHex(state.gitOid);
        const lineage = requested.get(key);
        if (lineage !== undefined && !sameLineage(lineage, state)) {
          throw new ProjectionStoreError(
            `Git object ${key} has ambiguous lineage in the fetch request`,
            409,
            "fetch-lineage-ambiguous",
          );
        }
        requested.set(key, state);
      }
    }
    const activeByGitOid = new Map<string, ActiveLineageRow[]>();
    const active = this.sql
      .exec<ActiveLineageRow>(
        `WITH newest AS (
           SELECT remote, bookmark, max(activation_seq) AS activation_seq
           FROM projection_states
           WHERE pending_batch_id IS NULL
           GROUP BY remote, bookmark
         )
         SELECT states.git_oid, states.canonical_commit_id, states.hidden_set_id
         FROM projection_states AS states
         JOIN newest USING (remote, bookmark, activation_seq)`,
      )
      .toArray();
    for (const state of active) {
      const key = toHex(new Uint8Array(state.git_oid));
      const states = activeByGitOid.get(key);
      if (states === undefined) activeByGitOid.set(key, [state]);
      else states.push(state);
    }
    for (const [gitOid, lineage] of requested) {
      for (const state of activeByGitOid.get(gitOid) ?? []) {
        if (
          !sameLineage(lineage, {
            canonicalCommitId: new Uint8Array(state.canonical_commit_id),
            hiddenSetId:
              state.hidden_set_id === null ? null : new Uint8Array(state.hidden_set_id),
          })
        ) {
          throw new ProjectionStoreError(
            `Git object ${gitOid} conflicts with active projection lineage`,
            409,
            "fetch-lineage-ambiguous",
          );
        }
      }
    }
  }

  private activeStateForObserved(remote: string, bookmark: string, gitOid: Uint8Array) {
    return this.sql
      .exec<ActiveStateRow>(
        `SELECT state_id, canonical_commit_id, hidden_set_id
         FROM projection_states
         WHERE pending_batch_id IS NULL AND remote = ? AND bookmark = ? AND git_oid = ?
         ORDER BY activation_seq DESC LIMIT 1`,
        remote,
        bookmark,
        exactBuffer(gitOid),
      )
      .toArray()[0];
  }

  private fetchResult(fetchId: ArrayBuffer) {
    return this.sql
      .exec<FetchResultRow>(
        `SELECT request_hash, activation_cursor
         FROM projection_fetch_results WHERE fetch_id = ?`,
        fetchId,
      )
      .toArray()[0];
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
        `SELECT next_fence, activation_cursor
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

function sameLineage(
  left: { canonicalCommitId: Uint8Array; hiddenSetId: Uint8Array | null },
  right: { canonicalCommitId: Uint8Array; hiddenSetId: Uint8Array | null },
) {
  return (
    equalBytes(left.canonicalCommitId, right.canonicalCommitId) &&
    compareNullableBytes(left.hiddenSetId, right.hiddenSetId) === 0
  );
}

function failure(error: unknown, status: number, code?: string) {
  return {
    ok: false as const,
    status,
    error: error instanceof Error ? error.message : "projection request failed",
    ...(code !== undefined
      ? { code }
      : error instanceof ProjectionStoreError && error.code !== undefined
        ? { code: error.code }
        : error instanceof ProjectionProtocolError
        ? { code: error.code }
        : {}),
  };
}

function defaultProjectionErrorCode(status: number): string {
  switch (status) {
    case 400:
      return "invalid-projection-request";
    case 403:
      return "projection-forbidden";
    case 404:
      return "projection-not-found";
    case 409:
      return "projection-conflict";
    case 429:
      return "projection-capacity-exhausted";
    default:
      throw new Error(`projection store error status ${status} requires an explicit code`);
  }
}

function requestFailure(error: unknown, code?: string) {
  return failure(
    error,
    error instanceof ProjectionStoreError ? error.status : 400,
    error instanceof ProjectionStoreError || error instanceof ProjectionProtocolError
      ? error.code
      : code,
  );
}

function remoteRequestFailure(error: unknown) {
  if (error instanceof RemoteProtocolError) return failure(error, 400, error.code);
  return failure(error, 400, "invalid-remote-request");
}

function requireAuthenticatedMachine(
  machineId: Uint8Array,
  authenticatedMachineId: string,
  code?: string,
) {
  if (toHex(machineId) !== authenticatedMachineId) {
    throw new ProjectionStoreError(
      "projection machine does not match authenticated machine",
      403,
      code ?? "projection-machine-mismatch",
    );
  }
}
