import { env, exports } from "cloudflare:workers";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { beforeAll, describe, expect, it } from "vitest";

import { gitToHex } from "../src/kernel";
import { decodeGitManifest } from "../src/pack_protocol";
import fixtures from "./fixtures/repository.json";

const defaultMachine = "a6".repeat(16);
const recoveryMachine = "b7".repeat(16);
const repositories = new Map<string, { repositoryId: string; incarnation: string }>();
let authorization: Record<string, string>;

beforeAll(() => {
  authorization = authorizationFor(defaultMachine);
});

describe("Git projection journal v2", () => {
  it("rejects missing canonical and missing public commits at begin", async () => {
    const repository = "git-journal-durability";
    const [canonicalOid, publicOid] = await installJournalFixture(repository);

    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          await incarnation(repository),
          "01".repeat(16),
          defaultMachine,
          projectionUpdate("missing-canonical", null, "f1".repeat(20), publicOid),
        ),
      ),
    ).toMatchObject({
      status: 409,
      body: {
        code: "projection-commit-not-durable",
        error: expect.stringContaining("canonical commit"),
      },
    });
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          await incarnation(repository),
          "02".repeat(16),
          defaultMachine,
          projectionUpdate("missing-public", null, canonicalOid, "f2".repeat(20)),
        ),
      ),
    ).toMatchObject({
      status: 409,
      body: {
        code: "projection-commit-not-durable",
        error: expect.stringContaining("public commit"),
      },
    });
  });

  it("replays an identity-bound batch only when the full request is unchanged", async () => {
    const repository = "git-journal-identity-retry";
    const [canonicalOid, publicOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const request = projectionBatch(
      repositoryIncarnation,
      "03".repeat(16),
      defaultMachine,
      projectionUpdate(
        "main",
        null,
        canonicalOid,
        publicOid,
        "13".repeat(64),
      ),
    );

    expect(await projectionRequest(repository, "pushes", request)).toEqual({
      status: 200,
      body: { pending: true, fence: 1 },
    });
    expect(await projectionRequest(repository, "pushes", request)).toEqual({
      status: 200,
      body: { pending: true, fence: 1 },
    });
    expect(
      await projectionRequest(repository, "pushes", {
        ...request,
        updates: request.updates.map((update) => ({
          ...update,
          states: update.states.map((state) => ({ ...state, hiddenSetId: null })),
        })),
      }),
    ).toMatchObject({ status: 409, body: { code: "projection-replay-mismatch" } });
  });

  it("persists across eviction and rejects stale owners and finished fences", async () => {
    const repository = "git-journal-fencing";
    const [canonicalOid, publicOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const batchId = "04".repeat(16);
    const request = projectionBatch(
      repositoryIncarnation,
      batchId,
      defaultMachine,
      projectionUpdate("main", null, canonicalOid, publicOid),
    );
    expect(await projectionRequest(repository, "pushes", request)).toMatchObject({
      status: 200,
      body: { fence: 1 },
    });

    await evictDurableObject(await repositoryGitStub(repository));
    expect(
      await projectionRequest(
        repository,
        `pushes/${batchId}/claim`,
        { incarnation: repositoryIncarnation, machineId: recoveryMachine },
        recoveryMachine,
      ),
    ).toEqual({ status: 200, body: { fence: 2, previousFence: 1 } });
    expect(await projectionReplay(repository, batchId)).toMatchObject({
      status: 200,
      body: { ownerMachine: recoveryMachine, fence: 2, updates: request.updates },
    });
    expect(
      await recover(repository, batchId, repositoryIncarnation, recoveryMachine, 2, [
        { bookmark: "main", liveOid: null },
      ]),
    ).toMatchObject({ status: 409, body: { code: "projection-replay-required" } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "main", liveOid: publicOid },
      ]),
    ).toMatchObject({ status: 409, body: { code: "projection-owner-stale" } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, recoveryMachine, 2, [
        { bookmark: "main", liveOid: publicOid },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, recoveryMachine, 1, [
        { bookmark: "main", liveOid: publicOid },
      ]),
    ).toMatchObject({ status: 409, body: { code: "projection-fence-stale" } });

    await evictDurableObject(await repositoryGitStub(repository));
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: { pending: [], cursors: [{ bookmark: "main", canonicalOid, publicOid }] },
    });
  });

  it("locks overlapping refs and aborts an unclaimed before-push batch", async () => {
    const repository = "git-journal-before-push";
    const [canonicalOid, publicOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const batchId = "05".repeat(16);
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          batchId,
          defaultMachine,
          projectionUpdate("main", null, canonicalOid, publicOid),
        ),
      ),
    ).toMatchObject({ status: 200, body: { pending: true, fence: 1 } });
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          "06".repeat(16),
          recoveryMachine,
          projectionUpdate("main", null, canonicalOid, publicOid),
        ),
        recoveryMachine,
      ),
    ).toMatchObject({ status: 409, body: { code: "push-in-progress" } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "main", liveOid: null },
      ]),
    ).toEqual({
      status: 200,
      body: { pending: false, fence: 1, outcome: "aborted" },
    });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: { activationCursor: 0, cursors: [], mappings: [], pending: [] },
    });
  });

  it("accepts observed public heads and clears cursors after accepted deletion", async () => {
    const repository = "git-journal-cursor-lifecycle";
    const [canonicalOid, publicOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const batchId = "07".repeat(16);
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          batchId,
          defaultMachine,
          projectionUpdate("main", null, canonicalOid, publicOid),
        ),
      ),
    ).toMatchObject({ status: 200, body: { fence: 1 } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "main", liveOid: publicOid },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: {
        activationCursor: 1,
        cursors: [{ bookmark: "main", canonicalOid, publicOid }],
        mappings: [{ bookmark: "main", canonicalOid, publicOid }],
      },
    });

    const deletionBatchId = "08".repeat(16);
    expect(
      await projectionRequest(repository, "pushes", {
        incarnation: repositoryIncarnation,
        batchId: deletionBatchId,
        machineId: defaultMachine,
        remote: "origin",
        updates: [
          {
            bookmark: "main",
            expectedOldOid: publicOid,
            states: [],
            proposedState: null,
            identityOid: null,
          },
        ],
      }),
    ).toMatchObject({ status: 200, body: { fence: 2 } });
    expect(
      await recover(repository, deletionBatchId, repositoryIncarnation, defaultMachine, 2, [
        { bookmark: "main", liveOid: null },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: { activationCursor: 1, cursors: [], pending: [] },
    });
  });

  it("quarantines mixed remote outcomes", async () => {
    const repository = "git-journal-mixed";
    const [canonicalOne, publicOne, canonicalTwo, publicTwo] =
      await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const batchId = "09".repeat(16);
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          batchId,
          defaultMachine,
          projectionUpdate("a", null, canonicalOne, publicOne),
          projectionUpdate("b", null, canonicalTwo, publicTwo),
        ),
      ),
    ).toMatchObject({ status: 200, body: { fence: 1 } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "a", liveOid: publicOne },
        { bookmark: "b", liveOid: null },
      ]),
    ).toMatchObject({
      status: 409,
      body: { code: "projection-remote-state-ambiguous" },
    });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: { activationCursor: 0, cursors: [], pending: [{ batchId }] },
    });
  });

  it("rejects one canonical OID binding to two public OIDs at begin", async () => {
    const repository = "git-journal-canonical-divergence";
    const [canonicalOid, publicOne, publicTwo] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          "0a".repeat(16),
          defaultMachine,
          projectionUpdate("a", null, canonicalOid, publicOne),
        ),
      ),
    ).toMatchObject({ status: 200, body: { pending: true } });
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          "0b".repeat(16),
          defaultMachine,
          projectionUpdate("b", null, canonicalOid, publicTwo),
        ),
      ),
    ).toMatchObject({ status: 409, body: { code: "canonical-oid-diverged" } });
  });

  it("records multiple canonical OIDs for one shared public OID", async () => {
    const repository = "git-journal-shared-public";
    const [canonicalOne, canonicalTwo, publicOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    await putRemote(repository, "origin", "/tmp/origin.git");
    const fetchId = "0c".repeat(16);
    const request = {
      incarnation: repositoryIncarnation,
      fetchId,
      machineId: defaultMachine,
      remote: "origin",
      refs: [
        fetchRef("a", publicOid, null, [projectionState(canonicalOne, publicOid)], 0),
        fetchRef("b", publicOid, null, [projectionState(canonicalTwo, publicOid)], 0),
      ],
    };

    expect(await projectionRequest(repository, "fetches", request)).toEqual({
      status: 200,
      body: { fetchId, activationCursor: 2 },
    });
    expect(await projectionRequest(repository, "fetches", request)).toEqual({
      status: 200,
      body: { fetchId, activationCursor: 2 },
    });
    const snapshot = await projectionSnapshot(repository);
    expect(snapshot).toMatchObject({
      status: 200,
      body: { activationCursor: 2 },
    });
    expect((snapshot.body as { cursors: unknown[] }).cursors).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ bookmark: "a", canonicalOid: canonicalOne, publicOid }),
        expect.objectContaining({ bookmark: "b", canonicalOid: canonicalTwo, publicOid }),
      ]),
    );
  });

  it("records an identity cursor without creating a projection state row", async () => {
    const repository = "git-journal-identity-cursor";
    const [identityOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    await putRemote(repository, "origin", "/tmp/origin.git");
    const fetchId = "1c".repeat(16);
    const request = {
      incarnation: repositoryIncarnation,
      fetchId,
      machineId: defaultMachine,
      remote: "origin",
      refs: [
        {
          ...fetchRef("main", identityOid, null, [], null),
          identityOid,
        },
      ],
    };

    expect(await projectionRequest(repository, "fetches", request)).toEqual({
      status: 200,
      body: { fetchId, activationCursor: 1 },
    });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: {
        activationCursor: 1,
        cursors: [
          {
            remote: "origin",
            bookmark: "main",
            canonicalOid: identityOid,
            publicOid: identityOid,
            hiddenSetId: null,
            activationSequence: 1,
          },
        ],
        mappings: [],
      },
    });
  });

  it("pushes identity history with cursor-only state and recovers the pending batch", async () => {
    const repository = "git-journal-identity-push";
    const [identityOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const batchId = "2c".repeat(16);
    const request = {
      incarnation: repositoryIncarnation,
      batchId,
      machineId: defaultMachine,
      remote: "origin",
      updates: [
        {
          bookmark: "main",
          expectedOldOid: null,
          states: [],
          proposedState: null,
          identityOid,
        },
      ],
    };

    expect(await projectionRequest(repository, "pushes", request)).toEqual({
      status: 200,
      body: { pending: true, fence: 1 },
    });
    expect(await projectionReplay(repository, batchId)).toMatchObject({
      status: 200,
      body: { updates: request.updates },
    });
    expect(
      await projectionRequest(
        repository,
        `pushes/${batchId}/claim`,
        { incarnation: repositoryIncarnation, machineId: recoveryMachine },
        recoveryMachine,
      ),
    ).toMatchObject({ status: 200, body: { fence: 2 } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, recoveryMachine, 2, [
        { bookmark: "main", liveOid: null },
      ]),
    ).toMatchObject({ status: 409, body: { code: "projection-replay-required" } });
    expect(await projectionReplay(repository, batchId)).toMatchObject({
      status: 200,
      body: { fence: 2, updates: request.updates },
    });
    expect(
      await recover(repository, batchId, repositoryIncarnation, recoveryMachine, 2, [
        { bookmark: "main", liveOid: identityOid },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: {
        activationCursor: 1,
        mappings: [],
        pending: [],
        cursors: [
          {
            bookmark: "main",
            canonicalOid: identityOid,
            publicOid: identityOid,
          },
        ],
      },
    });
    await runInDurableObject(await repositoryGitStub(repository), (_instance, state) => {
      for (const table of ["projection_git_states", "projection_git_receipts"]) {
        expect(
          state.storage.sql.exec<{ count: number }>(`SELECT count(*) AS count FROM ${table}`).one()
            .count,
          table,
        ).toBe(0);
      }
    });
  });

  it("rejects identity then rewritten and rewritten then identity canonical bindings", async () => {
    const identityFirst = "git-journal-identity-divergence-first";
    const [canonicalOid, publicOid] = await installJournalFixture(identityFirst);
    const identityIncarnation = await incarnation(identityFirst);
    const identityBatch = "2d".repeat(16);
    expect(
      await projectionRequest(identityFirst, "pushes", {
        incarnation: identityIncarnation,
        batchId: identityBatch,
        machineId: defaultMachine,
        remote: "origin",
        updates: [
          {
            bookmark: "identity",
            expectedOldOid: null,
            states: [],
            proposedState: null,
            identityOid: canonicalOid,
          },
        ],
      }),
    ).toMatchObject({ status: 200 });
    expect(
      await recover(identityFirst, identityBatch, identityIncarnation, defaultMachine, 1, [
        { bookmark: "identity", liveOid: canonicalOid },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });
    expect(
      await projectionRequest(
        identityFirst,
        "pushes",
        projectionBatch(
          identityIncarnation,
          "2e".repeat(16),
          defaultMachine,
          projectionUpdate("rewritten", null, canonicalOid, publicOid),
        ),
      ),
    ).toMatchObject({ status: 409, body: { code: "canonical-oid-diverged" } });

    const rewrittenFirst = "git-journal-rewritten-divergence-first";
    const [rewrittenCanonical, rewrittenPublic] = await installJournalFixture(rewrittenFirst);
    const rewrittenIncarnation = await incarnation(rewrittenFirst);
    expect(
      await projectionRequest(
        rewrittenFirst,
        "pushes",
        projectionBatch(
          rewrittenIncarnation,
          "2f".repeat(16),
          defaultMachine,
          projectionUpdate("rewritten", null, rewrittenCanonical, rewrittenPublic),
        ),
      ),
    ).toMatchObject({ status: 200 });
    await putRemote(rewrittenFirst, "origin", "/tmp/origin.git");
    expect(
      await projectionRequest(rewrittenFirst, "fetches", {
        incarnation: rewrittenIncarnation,
        fetchId: "30".repeat(16),
        machineId: defaultMachine,
        remote: "origin",
        refs: [
          {
            ...fetchRef("identity", rewrittenCanonical, null, [], null),
            identityOid: rewrittenCanonical,
          },
        ],
      }),
    ).toMatchObject({ status: 409, body: { code: "canonical-oid-diverged" } });
  });

  it("keeps canonical mapping receipts immutable after an aborted batch", async () => {
    const repository = "git-journal-receipt-immutability";
    const [canonicalOid, publicOne, publicTwo] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const batchId = "0d".repeat(16);
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          batchId,
          defaultMachine,
          projectionUpdate("a", null, canonicalOid, publicOne),
        ),
      ),
    ).toMatchObject({ status: 200, body: { fence: 1 } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "a", liveOid: null },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "aborted" } });
    expect(
      await projectionRequest(
        repository,
        "pushes",
        projectionBatch(
          repositoryIncarnation,
          "0e".repeat(16),
          defaultMachine,
          projectionUpdate("b", null, canonicalOid, publicTwo),
        ),
      ),
    ).toMatchObject({ status: 409, body: { code: "canonical-oid-diverged" } });
  });

  it("clears only a repointed remote and preserves repository-wide receipts", async () => {
    const repository = "git-journal-remote-change";
    const [canonicalOne, publicOne, canonicalTwo, publicTwo, canonicalThree, publicThree] =
      await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    await putRemote(repository, "origin", "/tmp/origin-one.git");
    await putRemote(repository, "backup", "/tmp/backup.git");

    const originBatch = "0f".repeat(16);
    expect(
      await projectionRequest(repository, "pushes", {
        ...projectionBatch(
          repositoryIncarnation,
          originBatch,
          defaultMachine,
          projectionUpdate("main", null, canonicalOne, publicOne),
        ),
        remote: "origin",
      }),
    ).toMatchObject({ status: 200, body: { fence: 1 } });
    expect(
      await recover(repository, originBatch, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "main", liveOid: publicOne },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });

    const backupBatch = "10".repeat(16);
    expect(
      await projectionRequest(repository, "pushes", {
        ...projectionBatch(
          repositoryIncarnation,
          backupBatch,
          defaultMachine,
          projectionUpdate("main", null, canonicalTwo, publicTwo),
        ),
        remote: "backup",
      }),
    ).toMatchObject({ status: 200, body: { fence: 2 } });
    expect(
      await recover(repository, backupBatch, repositoryIncarnation, defaultMachine, 2, [
        { bookmark: "main", liveOid: publicTwo },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });

    const pendingBatch = "11".repeat(16);
    expect(
      await projectionRequest(repository, "pushes", {
        ...projectionBatch(
          repositoryIncarnation,
          pendingBatch,
          defaultMachine,
          projectionUpdate("pending", null, canonicalThree, publicThree),
        ),
        remote: "origin",
      }),
    ).toMatchObject({ status: 200, body: { fence: 3 } });

    await putRemote(repository, "origin", "/tmp/origin-two.git");
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: {
        pending: [],
        cursors: [{ remote: "backup", bookmark: "main" }],
        mappings: [{ remote: "backup", bookmark: "main" }],
      },
    });
    expect(await projectionReplay(repository, pendingBatch)).toMatchObject({
      status: 404,
      body: { code: "projection-batch-not-found" },
    });
    expect(
      await projectionRequest(repository, "pushes", {
        ...projectionBatch(
          repositoryIncarnation,
          "12".repeat(16),
          defaultMachine,
          projectionUpdate("receipt", null, canonicalOne, publicTwo),
        ),
        remote: "backup",
      }),
    ).toMatchObject({ status: 409, body: { code: "canonical-oid-diverged" } });
    expect(await listRemotes(repository)).toEqual({
      status: 200,
      body: {
        remotes: [
          { name: "backup", url: "/tmp/backup.git" },
          { name: "origin", url: "/tmp/origin-two.git" },
        ],
      },
    });
  });

  it("pages accepted pair mappings under one fixed high-water", async () => {
    const repository = "git-journal-paging";
    const oids = await installJournalFixture(repository);
    expect(oids.length).toBeGreaterThanOrEqual(257);
    const repositoryIncarnation = await incarnation(repository);
    const states = oids.slice(0, 257).map((oid) => projectionState(oid, oid));
    const batchId = "13".repeat(16);
    expect(
      await projectionRequest(repository, "pushes", {
        incarnation: repositoryIncarnation,
        batchId,
        machineId: defaultMachine,
        remote: "origin",
        updates: [
          {
            bookmark: "main",
            expectedOldOid: null,
            states,
            proposedState: 256,
            identityOid: null,
          },
        ],
      }),
    ).toMatchObject({ status: 200, body: { fence: 1 } });
    expect(
      await recover(repository, batchId, repositoryIncarnation, defaultMachine, 1, [
        { bookmark: "main", liveOid: oids[256] },
      ]),
    ).toMatchObject({ status: 200, body: { outcome: "accepted" } });

    const first = await projectionSnapshot(repository);
    expect(first).toMatchObject({
      status: 200,
      body: { activationCursor: 257, nextAfter: 256, through: 257, hasMore: true },
    });
    expect((first.body as { mappings: unknown[] }).mappings).toHaveLength(256);
    const second = await projectionSnapshot(repository, 256, 257);
    expect(second).toMatchObject({
      status: 200,
      body: { nextAfter: 257, through: 257, hasMore: false },
    });
    expect((second.body as { mappings: unknown[] }).mappings).toHaveLength(1);
  });

  it("rejects malformed or missing hidden-set identities before journal mutation", async () => {
    const repository = "git-journal-hidden-identity";
    const [canonicalOid, publicOid] = await installJournalFixture(repository);
    const repositoryIncarnation = await incarnation(repository);
    const state = projectionState(canonicalOid, publicOid);
    const request = projectionBatch(
      repositoryIncarnation,
      "14".repeat(16),
      defaultMachine,
      {
        bookmark: "main",
        expectedOldOid: null,
        states: [state],
        proposedState: 0,
        identityOid: null,
      },
    );
    for (const hiddenSetId of ["aa", "AA".repeat(64), "gg".repeat(64)]) {
      expect(
        await projectionRequest(repository, "pushes", {
          ...request,
          updates: request.updates.map((update) => ({
            ...update,
            states: update.states.map((entry) => ({ ...entry, hiddenSetId })),
          })),
        }),
      ).toMatchObject({ status: 400, body: { code: "invalid-hidden-set-id" } });
    }
    const { hiddenSetId: _, ...missingIdentity } = state;
    expect(
      await projectionRequest(repository, "pushes", {
        ...request,
        updates: [{ ...request.updates[0], states: [missingIdentity] }],
      }),
    ).toMatchObject({ status: 400, body: { code: "invalid-hidden-set-id" } });
    expect(await projectionSnapshot(repository)).toMatchObject({
      status: 200,
      body: { activationCursor: 0, mappings: [], pending: [] },
    });
  });
});

interface EncodedFixture {
  id: string;
  manifest: string;
  chunks: string[];
}

async function ensureRepository(name: string) {
  const existing = repositories.get(name);
  if (existing !== undefined) return existing;
  const response = await exports.default.fetch(
    new Request("https://example.com/repositories", {
      method: "POST",
      headers: { ...authorization, "content-type": "application/json" },
      body: JSON.stringify({ name, idempotencyKey: randomHex(16) }),
    }),
  );
  if (!response.ok) throw new Error(`failed to create repository: ${await response.text()}`);
  const repository = (await response.json()) as { repositoryId: string; incarnation: string };
  repositories.set(name, repository);
  return repository;
}

async function incarnation(repository: string) {
  return (await ensureRepository(repository)).incarnation;
}

async function repositoryGitStub(repository: string) {
  const target = await ensureRepository(repository);
  return env.REPOSITORIES.getByName(target.repositoryId);
}

async function routeRequest(
  repository: string,
  path: string,
  init: RequestInit,
  machineId = defaultMachine,
) {
  const target = await ensureRepository(repository);
  return exports.default.fetch(
    new Request(`https://example.com/repositories/${target.repositoryId}/${path}`, {
      ...init,
      headers: {
        ...authorizationFor(machineId),
        "x-devspace-incarnation": target.incarnation,
        ...init.headers,
      },
    }),
  );
}

async function installJournalFixture(repository: string): Promise<string[]> {
  const fixture = (fixtures as { journal: EncodedFixture }).journal;
  const manifest = decodeHex(fixture.manifest);
  const chunks = fixture.chunks.map(decodeHex);
  expect(
    await json(
      await routeRequest(repository, `git/packs/${fixture.id}/manifest`, {
        method: "PUT",
        body: manifest,
      }),
    ),
  ).toMatchObject({ inserted: true });
  for (const [position, chunk] of chunks.entries()) {
    expect(
      await json(
        await routeRequest(repository, `git/packs/${fixture.id}/chunks/${position}`, {
          method: "PUT",
          body: chunk,
        }),
      ),
    ).toMatchObject({ inserted: true });
  }
  expect(
    await json(
      await routeRequest(repository, `git/packs/${fixture.id}/install`, { method: "POST" }),
    ),
  ).toMatchObject({ installed: true });
  return decodeGitManifest(manifest).headCommits.map(gitToHex);
}

async function projectionRequest(
  repository: string,
  path: string,
  body: unknown,
  machineId = defaultMachine,
) {
  const response = await routeRequest(
    repository,
    `git/projection/${path}`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    },
    machineId,
  );
  return { status: response.status, body: await response.json() };
}

async function projectionSnapshot(repository: string, after = 0, through?: number) {
  const highWater = through === undefined ? "" : `&through=${through}`;
  const response = await routeRequest(
    repository,
    `git/projection?incarnation=${await incarnation(repository)}&after=${after}${highWater}`,
    { method: "GET" },
  );
  return { status: response.status, body: await response.json() };
}

async function projectionReplay(repository: string, batchId: string) {
  const response = await routeRequest(
    repository,
    `git/projection/pushes/${batchId}/replay?incarnation=${await incarnation(repository)}`,
    { method: "GET" },
  );
  return { status: response.status, body: await response.json() };
}

function recover(
  repository: string,
  batchId: string,
  repositoryIncarnation: string,
  machineId: string,
  fence: number,
  observations: Array<{ bookmark: string; liveOid: string | null }>,
) {
  return projectionRequest(
    repository,
    `pushes/${batchId}/recover`,
    { incarnation: repositoryIncarnation, machineId, fence, observations },
    machineId,
  );
}

async function putRemote(repository: string, name: string, url: string) {
  const response = await routeRequest(
    repository,
    `git/remotes/${encodeURIComponent(name)}`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ incarnation: await incarnation(repository), url }),
    },
  );
  const result = { status: response.status, body: await response.json() };
  expect(result).toMatchObject({ status: 200 });
  return result;
}

async function listRemotes(repository: string) {
  const response = await routeRequest(
    repository,
    `git/remotes?incarnation=${await incarnation(repository)}`,
    { method: "GET" },
  );
  return { status: response.status, body: await response.json() };
}

function projectionState(
  canonicalOid: string,
  publicOid: string,
  hiddenSetId: string | null = null,
) {
  return { canonicalOid, publicOid, hiddenSetId };
}

function projectionUpdate(
  bookmark: string,
  expectedOldOid: string | null,
  canonicalOid: string,
  publicOid: string,
  hiddenSetId: string | null = null,
) {
  return {
    bookmark,
    expectedOldOid,
    states: [projectionState(canonicalOid, publicOid, hiddenSetId)],
    proposedState: 0,
    identityOid: null,
  };
}

function projectionBatch(
  repositoryIncarnation: string,
  batchId: string,
  machineId: string,
  ...updates: ReturnType<typeof projectionUpdate>[]
) {
  return { incarnation: repositoryIncarnation, batchId, machineId, remote: "origin", updates };
}

function fetchRef(
  bookmark: string,
  observedPublicOid: string,
  expectedCursorOid: string | null,
  states: ReturnType<typeof projectionState>[],
  proposedState: number | null,
) {
  return {
    bookmark,
    observedPublicOid,
    expectedCursorOid,
    states,
    proposedState,
    identityOid: null,
  };
}

function authorizationFor(machineId: string): Record<string, string> {
  return {
    authorization: `Bearer ${env.DEVSPACE_SHARED_SECRET}`,
    "x-devspace-machine-id": machineId,
  };
}

function randomHex(bytes: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function decodeHex(value: string): Uint8Array {
  return Uint8Array.from({ length: value.length / 2 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

async function json(response: Response): Promise<unknown> {
  expect(response.status).toBe(200);
  return response.json();
}
