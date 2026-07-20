import { env, exports } from "cloudflare:workers";
import { evictDurableObject } from "cloudflare:test";
import { beforeAll, describe, expect, it } from "vitest";
import jjGolden from "../crates/kernel/tests/jj_golden.txt?raw";
import { MAX_HEAD_REQUEST_BYTES, MAX_OBSERVED_HEADS } from "../src/head_protocol";
import { KIND, Kernel, isKindName, toHex } from "../src/kernel";
import {
  MAX_OBJECT_INVENTORY_KEYS,
  MAX_OBJECT_INVENTORY_REQUEST_BYTES,
} from "../src/object_protocol";
import { MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, decodeManifest } from "../src/pack_protocol";

const defaultMachine = "66".repeat(16);
const recoveryMachine = "77".repeat(16);
const repositories = new Map<
  string,
  { repositoryId: string; incarnation: string; logicalIncarnation: string }
>();
let authorization: Record<string, string>;

beforeAll(async () => {
  authorization = authorizationFor(defaultMachine);
});
const helloId =
  "e4cfa39a3d37be31c59609e807970799caa68a19bfaa15135f165085e01d41a65ba1e1b146aeb6bd0092b49eac214c103ccfa3a365954bbbe52f74a2b3620c94";
const helloPackId =
  "606591ef0c95a0b8ab99b4ccc8cfd34f05e143f82cf4e7ff0766183d21f0fce42456f1d602deaaef70fcaed78de2ca8cee73a055853d7aff1409c7a26b185733";
const hello = new TextEncoder().encode("hello");
const headsPackId =
  "f1bf19025a446aefff8403fb0fdee17ff43382ecc8d5df0d398f24a741025c06faa832335491098ab8a285fce47d49d13510b94a7d009e4cefa3061a892ce52a";
const zeroId = "00".repeat(64);

describe("validation kernel", () => {
  it("matches jj IDs for every object kind through Wasm", () => {
    const kernel = new Kernel();
    for (const line of jjGolden.trim().split("\n")) {
      const [kind, expectedId, encoded] = line.split("|");
      if (!isKindName(kind)) throw new Error(`unknown golden kind ${kind}`);
      const result = kernel.validate(KIND[kind], decodeRle(encoded));
      expect(toHex(result.id), `${kind} ID`).toBe(expectedId);
    }
  });

  it("extracts references and recovers after malformed protobuf", () => {
    const kernel = new Kernel();
    const fileId = new Uint8Array(64).fill(7);
    const tree = kernel.validate(
      KIND.tree,
      treeWithFiles([
        ["data", fileId],
        ["same", fileId],
      ]),
    );
    expect(tree.references).toHaveLength(1);
    expect(tree.references[0].kind).toBe(KIND.file);
    expect(toHex(tree.references[0].id)).toBe("07".repeat(64));
    expect(() => kernel.validate(KIND.commit, new Uint8Array([0xff, 0xff]))).toThrow(
      "decode commit",
    );
    expect(toHex(kernel.validate(KIND.tree, new Uint8Array()).id)).toBe(
      "482ae5a29fbe856c7272f2071b8b0f0359ee2d89ff392b8a900643fbd0836eccd067b8bf41909e206c90d45d6e7d8b6686b93ecaee5fe1a9060d87b672101310",
    );
  });
});

describe("pack installation", () => {
  it("quarantines, validates and atomically installs a pack idempotently", async () => {
    const manifest = helloManifest();
    expect(decodeManifest(manifest)).toMatchObject({ packLength: 5 });

    const firstManifest = await putManifest("pack", helloPackId, manifest);
    expect(firstManifest.status).toBe(200);
    expect(await firstManifest.json()).toEqual({ inserted: true, installed: false });

    const earlyInstall = await install("pack", helloPackId);
    expect(earlyInstall.status).toBe(400);
    expect(await earlyInstall.json()).toEqual({ error: "pack is missing 1 chunks" });

    const corruptChunk = await putChunk(
      "pack",
      helloPackId,
      0,
      new TextEncoder().encode("HELLO"),
    );
    expect(corruptChunk.status).toBe(400);
    expect(await corruptChunk.json()).toEqual({ error: "chunk 0 hash does not match manifest" });

    const firstChunk = await putChunk("pack", helloPackId, 0, hello);
    expect(await firstChunk.json()).toEqual({ inserted: true, installed: false });
    const secondChunk = await putChunk("pack", helloPackId, 0, hello);
    expect(await secondChunk.json()).toEqual({ inserted: false, installed: false });

    const installed = await install("pack", helloPackId);
    expect(installed.status).toBe(200);
    expect(await installed.json()).toEqual({ installed: true, insertedObjects: 1 });
    const stub = await repositoryStub("pack");
    expect(await stub.countObjects()).toBe(1);
    expect(await stub.countInstalledPacks()).toBe(1);

    expect(await (await install("pack", helloPackId)).json()).toEqual({
      installed: false,
      insertedObjects: 0,
    });
    expect(await (await putManifest("pack", helloPackId, manifest)).json()).toEqual({
      inserted: false,
      installed: true,
    });
    expect(await (await putChunk("pack", helloPackId, 0, hello)).json()).toEqual({
      inserted: false,
      installed: true,
    });
    expect((await putChunk("pack", helloPackId, 0, new TextEncoder().encode("HELLO"))).status).toBe(
      400,
    );
    expect((await putChunk("pack", helloPackId, 1, hello)).status).toBe(400);
  });

  it("survives eviction between quarantine and install", async () => {
    const stub = await repositoryStub("eviction");
    const authority = await repositoryAuthority("eviction");
    await stub.putPackManifest(authority, helloPackId, helloManifest());
    await stub.putPackChunk(authority, helloPackId, 0, hello);
    await evictDurableObject(stub);

    const reloaded = await repositoryStub("eviction");
    expect(await reloaded.installPack(authority, helloPackId)).toEqual({
      ok: true,
      installed: true,
      insertedObjects: 1,
    });
    expect(await reloaded.countObjects()).toBe(1);
    await evictDurableObject(reloaded);
    expect(await (await repositoryStub("eviction")).putPackManifest(authority, helloPackId, helloManifest()))
      .toEqual({ ok: true, inserted: false, installed: true });
  });

  it("allows references to arrive in another pack before head advancement", async () => {
    const referencedFile = new Uint8Array(64).fill(7);
    const tree = treeWithFiles([["later", referencedFile]]);
    const fixture = singleObjectPack(2, tree);

    expect((await putManifest("cross-pack", fixture.id, fixture.manifest)).status).toBe(200);
    expect((await putChunk("cross-pack", fixture.id, 0, tree)).status).toBe(200);
    expect(await (await install("cross-pack", fixture.id)).json()).toEqual({
      installed: true,
      insertedObjects: 1,
    });
    expect(await (await repositoryStub("cross-pack")).countObjects()).toBe(1);
  });

  it("rolls back earlier object inserts when a later object is invalid", async () => {
    const fixture = rollbackFixture();
    expect((await putManifest("rollback", fixture.id, fixture.manifest)).status).toBe(200);
    expect((await putChunk("rollback", fixture.id, 0, fixture.bytes)).status).toBe(200);
    const response = await install("rollback", fixture.id);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: "object 1 ID does not match manifest" });
    const stub = await repositoryStub("rollback");
    expect(await stub.countObjects()).toBe(0);
    expect(await stub.countInstalledPacks()).toBe(0);
  });

  it("accepts the frozen heads-carrying multi-chunk manifest vector", async () => {
    const kernel = new Kernel();
    const manifest = headsManifest();
    expect(toHex(kernel.hash([manifest]))).toBe(headsPackId);
    expect(decodeManifest(manifest)).toMatchObject({
      chunkBytes: 64 * 1024,
      packLength: 80 * 1024,
    });
    expect(decodeManifest(manifest).operationHeads).toHaveLength(2);
    const response = await putManifest("heads-vector", headsPackId, manifest);
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ inserted: true, installed: false });
  });

  it("rejects noncanonical manifests before quarantine", async () => {
    const manifest = helloManifest();
    manifest[6] = 1;
    expect(() => decodeManifest(manifest)).toThrow("reserved bytes must be zero");
    const response = await putManifest("bad-manifest", helloPackId, manifest);
    expect(response.status).toBe(400);
    expect(await response.json()).toMatchObject({ error: expect.stringContaining("hashes to") });
    expect(await (await repositoryStub("bad-manifest")).countInstalledPacks()).toBe(0);
  });

  it("enforces authentication, identifiers, routes and body bounds", async () => {
    const unauthorized = await exports.default.fetch(
      new Request(`https://example.com/repositories/repo/packs/${helloPackId}/manifest`, {
        method: "PUT",
        body: helloManifest(),
      }),
    );
    expect(unauthorized.status).toBe(401);
    const invalidName = await exports.default.fetch(
      new Request("https://example.com/repositories", {
        method: "POST",
        headers: { ...authorization, "content-type": "application/json" },
        body: JSON.stringify({ name: "UPPER", idempotencyKey: "10".repeat(16) }),
      }),
    );
    expect(invalidName.status).toBe(400);
    expect((await putManifest("repo", "bad", helloManifest())).status).toBe(400);
    expect((await putChunk("repo", helloPackId, -1, hello)).status).toBe(400);
    const oldObjectRoute = await exports.default.fetch(
      new Request("https://example.com/repositories/repo/objects/file", {
        method: "PUT",
        headers: authorization,
        body: hello,
      }),
    );
    expect(oldObjectRoute.status).toBe(404);
    expect(() => new Kernel().validate(99, new Uint8Array())).toThrow("unknown object kind");

    const oversizedManifest = await putManifest(
      "body-limit",
      helloPackId,
      new Uint8Array(MAX_MANIFEST_BYTES + 1),
    );
    expect(oversizedManifest.status).toBe(400);
    expect(await oversizedManifest.json()).toEqual({
      error: `manifest exceeds ${MAX_MANIFEST_BYTES} byte limit`,
    });
    const oversizedChunk = await putChunk(
      "body-limit",
      helloPackId,
      0,
      new Uint8Array(MAX_CHUNK_BYTES + 1),
    );
    expect(oversizedChunk.status).toBe(400);
    expect(await oversizedChunk.json()).toEqual({
      error: `chunk exceeds ${MAX_CHUNK_BYTES} byte limit`,
    });
  });
});

describe("cloud operation heads", () => {
  const incarnation = "11".repeat(16);
  const otherIncarnation = "22".repeat(16);

  it("preserves unseen concurrent heads and replays the exact idempotent result", async () => {
    const repository = "head-convergence";
    expect(await initialize(repository, incarnation)).toEqual({
      status: 200,
      body: { initialized: true, cursor: 0, heads: [] },
    });
    expect(await initialize(repository, incarnation)).toEqual({
      status: 200,
      body: { initialized: false, cursor: 0, heads: [] },
    });
    expect(await initialize(repository, otherIncarnation)).toEqual({
      status: 409,
      body: { error: "repository incarnation does not match" },
    });

    const base = await installOperation(repository, "base");
    const left = await installOperation(repository, "left", [base]);
    const right = await installOperation(repository, "right", [base]);
    const merged = await installOperation(repository, "merged", [left, right]);
    const unrelated = await installOperation(repository, "unrelated");

    expect(await headTransaction(repository, incarnation, "01".repeat(16), base, [])).toEqual({
      status: 200,
      body: { cursor: 1, heads: [base] },
    });
    const leftRequest = headRequest(incarnation, "02".repeat(16), left, [base]);
    expect(await postHeads(repository, leftRequest)).toEqual({
      status: 200,
      body: { cursor: 2, heads: [left] },
    });
    expect(await headTransaction(repository, incarnation, "03".repeat(16), right, [base])).toEqual({
      status: 200,
      body: { cursor: 3, heads: [left, right].sort() },
    });

    expect(await postHeads(repository, leftRequest)).toEqual({
      status: 200,
      body: { cursor: 2, heads: [left] },
    });
    expect(
      await headTransaction(repository, incarnation, "02".repeat(16), merged, [left, right]),
    ).toEqual({
      status: 409,
      body: { error: "idempotency key was already used for a different head request" },
    });
    expect(await headTransaction(repository, incarnation, "04".repeat(16), merged, [right, left]))
      .toEqual({ status: 200, body: { cursor: 4, heads: [merged] } });
    expect(await headTransaction(repository, incarnation, "09".repeat(16), unrelated, [merged]))
      .toEqual({
        status: 409,
        body: { error: `observed current head is not an ancestor of new head: ${merged}` },
      });
    expect(await getHeads(repository, incarnation)).toEqual({
      status: 200,
      body: { cursor: 4, heads: [merged] },
    });
    expect(await getHeads(repository, otherIncarnation)).toEqual({
      status: 409,
      body: { error: "repository incarnation does not match" },
    });
    expect(
      await headTransaction(repository, otherIncarnation, "05".repeat(16), left, [merged]),
    ).toEqual({
      status: 409,
      body: { error: "repository incarnation does not match" },
    });
  });

  it("rejects an incomplete closure without consuming its idempotency key", async () => {
    const repository = "head-closure";
    await initialize(repository, incarnation);
    const stub = await repositoryStub(repository, incarnation);
    const view = canonicalEmptyView();
    const viewId = toHex(new Kernel().validate(KIND.view, view).id);
    const parent = canonicalOperation(fromHex(viewId), "needs view");
    const parentId = toHex(new Kernel().validate(KIND.operation, parent).id);
    const child = canonicalOperation(new Uint8Array(64), "needs parent", [fromHex(parentId)]);
    const childId = await installObject(repository, KIND.operation, child);
    const request = headRequest(incarnation, "06".repeat(16), childId, []);

    expect(await postHeads(repository, request)).toEqual({
      status: 409,
      body: { error: `head closure is missing operation ${parentId}` },
    });
    expect(await getHeads(repository, incarnation)).toEqual({
      status: 200,
      body: { cursor: 0, heads: [] },
    });

    expect(await installObject(repository, KIND.operation, parent)).toBe(parentId);
    expect(await postHeads(repository, request)).toEqual({
      status: 409,
      body: { error: `head closure is missing view ${viewId}` },
    });
    expect(await installObject(repository, KIND.view, view)).toBe(viewId);
    expect(await postHeads(repository, request)).toEqual({
      status: 200,
      body: { cursor: 1, heads: [childId] },
    });
    await evictDurableObject(stub);
    expect(await getHeads(repository, incarnation)).toEqual({
      status: 200,
      body: { cursor: 1, heads: [childId] },
    });
  });

  it("validates the bounded canonical head request surface", async () => {
    const repository = "head-validation";
    const missing = await exports.default.fetch(
      new Request(`https://example.com/repositories/${"00".repeat(32)}/heads`, {
        headers: {
          ...authorization,
          "x-devspace-incarnation": incarnation,
        },
      }),
    );
    expect(missing.status).toBe(404);
    expect(await missing.json()).toEqual({ error: "repository not found" });
    await initialize(repository, incarnation);
    const operation = await installOperation(repository, "validation");
    expect(
      await postHeads(repository, {
        ...headRequest(incarnation, "07".repeat(16), operation, []),
        unexpected: true,
      }),
    ).toEqual({
      status: 400,
      body: {
        error: "head request fields must be exactly incarnation, idempotencyKey, newHead, observedHeads",
      },
    });
    expect(
      await postHeads(repository, headRequest(incarnation, "07".repeat(16), operation, [zeroId, zeroId])),
    ).toEqual({
      status: 400,
      body: { error: "observedHeads must not contain duplicates" },
    });
    expect(await postRawHeads(repository, "{")).toEqual({
      status: 400,
      body: { error: "head request must be valid JSON" },
    });
    expect(
      await postHeads(
        repository,
        headRequest(
          incarnation,
          "08".repeat(16),
          operation,
          Array.from({ length: MAX_OBSERVED_HEADS + 1 }, (_, index) =>
            index.toString(16).padStart(128, "0"),
          ),
        ),
      ),
    ).toEqual({
      status: 400,
      body: { error: `observedHeads exceeds the ${MAX_OBSERVED_HEADS}-head limit` },
    });
    expect(await postRawHeads(repository, "x".repeat(MAX_HEAD_REQUEST_BYTES + 1))).toEqual({
      status: 400,
      body: { error: `head request exceeds ${MAX_HEAD_REQUEST_BYTES} byte limit` },
    });
  });
});

describe("cloud pack download", () => {
  const incarnation = "33".repeat(16);

  it("reconstructs a chunk with a zero-length object in its byte range", async () => {
    const repository = "pack-download-zero-length";
    await initialize(repository, incarnation);
    const fixture = zeroLengthMiddleObjectPack();
    expect((await putManifest(repository, fixture.id, fixture.manifest)).status).toBe(200);
    expect((await putChunk(repository, fixture.id, 0, fixture.chunk)).status).toBe(200);
    expect((await install(repository, fixture.id)).status).toBe(200);

    expect(await downloadPackManifest(repository, fixture.id, incarnation)).toEqual(
      fixture.manifest,
    );
    expect(await downloadPackChunk(repository, fixture.id, 0, incarnation)).toEqual(
      fixture.chunk,
    );
  });

  it("lists installed packs and reproduces their exact manifest and chunks", async () => {
    const repository = "pack-download";
    await initialize(repository, incarnation);
    const manifest = helloManifest();
    expect((await putManifest(repository, helloPackId, manifest)).status).toBe(200);
    expect((await putChunk(repository, helloPackId, 0, hello)).status).toBe(200);
    expect((await install(repository, helloPackId)).status).toBe(200);

    expect(await listPacks(repository, incarnation, 0)).toEqual({
      status: 200,
      body: {
        packs: [{ sequence: 1, id: helloPackId }],
        nextAfter: 1,
        through: 1,
        hasMore: false,
      },
    });
    expect(await listPacks(repository, incarnation, 1, 1)).toEqual({
      status: 200,
      body: { packs: [], nextAfter: 1, through: 1, hasMore: false },
    });
    expect(await listPacks(repository, incarnation, 1, 99)).toEqual({
      status: 400,
      body: {
        error: "pack high-water must be between the cursor and current catalog frontier",
      },
    });
    expect(await downloadPackManifest(repository, helloPackId, incarnation)).toEqual(manifest);
    expect(await downloadPackChunk(repository, helloPackId, 0, incarnation)).toEqual(hello);

    const spanning = spanningObjectPack();
    expect((await putManifest(repository, spanning.id, spanning.manifest)).status).toBe(200);
    for (const [position, chunk] of spanning.chunks.entries()) {
      expect((await putChunk(repository, spanning.id, position, chunk)).status).toBe(200);
    }
    expect((await install(repository, spanning.id)).status).toBe(200);
    expect(await listPacks(repository, incarnation, 1, 1)).toEqual({
      status: 200,
      body: { packs: [], nextAfter: 1, through: 1, hasMore: false },
    });
    expect(await listPacks(repository, incarnation, 1)).toEqual({
      status: 200,
      body: {
        packs: [{ sequence: 2, id: spanning.id }],
        nextAfter: 2,
        through: 2,
        hasMore: false,
      },
    });
    expect(await downloadPackManifest(repository, spanning.id, incarnation)).toEqual(
      spanning.manifest,
    );
    for (const [position, chunk] of spanning.chunks.entries()) {
      expect(await downloadPackChunk(repository, spanning.id, position, incarnation)).toEqual(
        chunk,
      );
    }

    expect(await listPacks(repository, "44".repeat(16), 0)).toEqual({
      status: 409,
      body: { error: "repository incarnation does not match" },
    });
    const missing = await fetchPackChunk(repository, "00".repeat(64), 0, incarnation);
    expect(missing.status).toBe(404);
    expect(await missing.json()).toEqual({ error: "installed pack chunk does not exist" });
  });
});

describe("cloud object inventory", () => {
  const incarnation = "34".repeat(16);

  it("returns the installed subset of a bounded canonical candidate set", async () => {
    const repository = "object-inventory";
    await initialize(repository, incarnation);
    expect(await installObject(repository, KIND.file, hello)).toBe(helloId);
    const missingFile = "ff".repeat(64);
    const missingOperation = "01".repeat(64);

    expect(
      await objectInventory(repository, {
        incarnation,
        objects: [
          { kind: KIND.file, id: helloId },
          { kind: KIND.file, id: missingFile },
          { kind: KIND.operation, id: missingOperation },
        ],
      }),
    ).toEqual({
      status: 200,
      body: { objects: [{ kind: KIND.file, id: helloId }] },
    });
    expect(
      await objectInventory(repository, {
        incarnation: "35".repeat(16),
        objects: [{ kind: KIND.file, id: helloId }],
      }),
    ).toEqual({
      status: 409,
      body: { error: "repository incarnation does not match" },
    });
  });

  it("rejects noncanonical, oversized and inexact inventory requests", async () => {
    const repository = "object-inventory-validation";
    await initialize(repository, incarnation);
    const stub = await repositoryStub(repository, incarnation);
    expect(
      await objectInventory(repository, {
        incarnation,
        objects: [{ kind: KIND.operation, id: "01".repeat(64) }],
        unexpected: true,
      }),
    ).toEqual({
      status: 400,
      body: {
        error: "object inventory request fields must be exactly incarnation, objects",
      },
    });
    expect(
      await objectInventory(repository, {
        incarnation,
        objects: [
          { kind: KIND.operation, id: "02".repeat(64) },
          { kind: KIND.operation, id: "01".repeat(64) },
        ],
      }),
    ).toEqual({
      status: 400,
      body: { error: "objects must be strictly sorted and unique" },
    });
    expect(
      await stub.inventoryObjects(await repositoryAuthority(repository), {
        incarnation,
        objects: Array.from({ length: MAX_OBJECT_INVENTORY_KEYS + 1 }, (_, index) => ({
          kind: KIND.file,
          id: index.toString(16).padStart(128, "0"),
        })),
      }),
    ).toMatchObject({
      ok: false,
      status: 400,
      error: `objects exceeds the ${MAX_OBJECT_INVENTORY_KEYS}-object limit`,
    });
    expect(
      await objectInventoryRaw(
        repository,
        new TextEncoder().encode("x".repeat(MAX_OBJECT_INVENTORY_REQUEST_BYTES + 1)),
      ),
    ).toEqual({
      status: 400,
      body: {
        error: `object inventory request exceeds ${MAX_OBJECT_INVENTORY_REQUEST_BYTES} byte limit`,
      },
    });
  });
});

describe("Git projection journal", () => {
  const incarnation = "55".repeat(16);
  const firstMachine = "66".repeat(16);
  const recoveryMachine = "77".repeat(16);

  it("binds projection ownership to the authenticated machine", async () => {
    const repository = "projection-machine-auth";
    await initializeProjectionRepository(repository, incarnation);
    const target = await ensureRepository(repository, incarnation);
    const response = await repositoryRequest(
      repository,
      "git/pushes",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          incarnation: target.incarnation,
          batchId: "79".repeat(16),
          machineId: recoveryMachine,
          remote: "origin",
          updates: [
            {
              bookmark: "main",
              expectedOldOid: null,
              states: [
                {
                  gitOid: "01".repeat(20),
                  canonicalCommitId: "01".repeat(64),
                  publicCommitId: "02".repeat(64),
                  hiddenSetId: null,
                },
              ],
              proposedState: 0,
            },
          ],
        }),
      },
      firstMachine,
    );
    expect(response.status).toBe(403);
    expect(await response.json()).toEqual({
      error: "projection machine does not match authenticated machine",
    });
  });

  it("recovers an evicted post-push batch under a new fencing token", async () => {
    const repository = "projection-recovery";
    const batchId = "81".repeat(16);
    const gitOid = "91".repeat(20);
    const hiddenSetId = "a5".repeat(64);
    await initializeProjectionRepository(repository, incarnation);
    const [canonicalCommitId, publicCommitId] = await installProjectionCommits(repository);

    const begin = projectionBatch(
      incarnation,
      batchId,
      firstMachine,
      projectionUpdate("main", null, gitOid, canonicalCommitId, publicCommitId, hiddenSetId),
    );
    expect(await projectionRequest(repository, "git/pushes", begin)).toEqual({
      status: 200,
      body: { pending: true, fence: 1 },
    });
    expect(await getProjection(repository, incarnation)).toEqual({
      status: 200,
      body: {
        activationCursor: 0,
        cursors: [],
        mappings: [],
        nextAfter: 0,
        through: 0,
        hasMore: false,
        pending: [
          {
            batchId,
            remote: "origin",
            ownerMachine: firstMachine,
            fence: 1,
            refs: [
              {
                bookmark: "main",
                expectedOldOid: null,
                proposedGitOid: gitOid,
              },
            ],
          },
        ],
      },
    });

    await evictDurableObject(await repositoryStub(repository, incarnation));
    expect(
      await projectionRequest(repository, `git/pushes/${batchId}/claim`, {
        incarnation,
        machineId: recoveryMachine,
      }),
    ).toEqual({ status: 200, body: { fence: 2, previousFence: 1 } });
    expect(await getProjectionReplay(repository, batchId, incarnation)).toEqual({
      status: 200,
      body: {
        batchId,
        remote: "origin",
        ownerMachine: recoveryMachine,
        fence: 2,
        updates: begin.updates,
      },
    });
    expect(
      await projectionRequest(repository, `git/pushes/${batchId}/recover`, {
        incarnation,
        machineId: recoveryMachine,
        fence: 2,
        observations: [{ bookmark: "main", liveOid: null }],
      }),
    ).toEqual({
      status: 409,
      body: {
        error:
          "claimed projection batch still matches its expected refs; replay the exact push before recovery",
      },
    });
    expect(
      await projectionRequest(repository, `git/pushes/${batchId}/confirm`, {
        incarnation,
        machineId: firstMachine,
        fence: 1,
      }),
    ).toEqual({
      status: 409,
      body: { error: "projection owner or fencing token is stale" },
    });
    expect(
      await projectionRequest(repository, `git/pushes/${batchId}/confirm`, {
        incarnation,
        machineId: recoveryMachine,
        fence: 2,
      }),
    ).toEqual({
      status: 409,
      body: { error: "claimed projection batch requires observed remote state recovery" },
    });
    expect(
      await projectionRequest(repository, `git/pushes/${batchId}/recover`, {
        incarnation,
        machineId: recoveryMachine,
        fence: 2,
        observations: [{ bookmark: "main", liveOid: gitOid }],
      }),
    ).toEqual({
      status: 200,
      body: { pending: false, fence: 2, outcome: "accepted" },
    });
    expect((await getProjection(repository, incarnation)).body).toMatchObject({
      activationCursor: 1,
      pending: [],
      cursors: [
        {
          remote: "origin",
          bookmark: "main",
          gitOid,
          canonicalCommitId,
          publicCommitId,
          hiddenSetId,
          activationSequence: 1,
        },
      ],
    });
    expect(await projectionRequest(repository, "git/pushes", begin)).toEqual({
      status: 200,
      body: { pending: false, fence: 2, outcome: "accepted" },
    });
    expect(
      await projectionRequest(repository, "git/pushes", {
        ...begin,
        updates: begin.updates.map((update) => ({
          ...update,
          states: update.states.map((state) => ({ ...state, hiddenSetId: null })),
        })),
      }),
    ).toEqual({
      status: 409,
      body: { error: "projection batch ID was already used for a different request" },
    });

    const deletionBatch = "8a".repeat(16);
    expect(
      await projectionRequest(repository, "git/pushes", {
        incarnation,
        batchId: deletionBatch,
        machineId: firstMachine,
        remote: "origin",
        updates: [
          { bookmark: "main", expectedOldOid: gitOid, states: [], proposedState: null },
        ],
      }),
    ).toEqual({ status: 200, body: { pending: true, fence: 3 } });
    expect(
      await projectionRequest(repository, `git/pushes/${deletionBatch}/claim`, {
        incarnation,
        machineId: recoveryMachine,
      }),
    ).toEqual({ status: 200, body: { fence: 4, previousFence: 3 } });
    expect(
      await projectionRequest(repository, `git/pushes/${deletionBatch}/recover`, {
        incarnation,
        machineId: recoveryMachine,
        fence: 4,
        observations: [{ bookmark: "main", liveOid: null }],
      }),
    ).toEqual({
      status: 200,
      body: { pending: false, fence: 4, outcome: "accepted" },
    });
    expect((await getProjection(repository, incarnation)).body).toMatchObject({ cursors: [] });
  });

  it("aborts an unapplied batch and retains mixed remote outcomes", async () => {
    const repository = "projection-classify";
    await initializeProjectionRepository(repository, incarnation);
    const [canonicalCommitId, publicCommitId] = await installProjectionCommits(repository);
    const firstBatch = "82".repeat(16);
    const firstGitOid = "92".repeat(20);
    const secondGitOid = "93".repeat(20);
    const begin = projectionBatch(
      incarnation,
      firstBatch,
      firstMachine,
      projectionUpdate("a", null, firstGitOid, canonicalCommitId, publicCommitId),
      projectionUpdate("B", null, secondGitOid, canonicalCommitId, publicCommitId),
    );
    expect((await projectionRequest(repository, "git/pushes", begin)).status).toBe(200);
    expect(
      await projectionRequest(
        repository,
        "git/pushes",
        projectionBatch(
          incarnation,
          "83".repeat(16),
          recoveryMachine,
          projectionUpdate("a", null, firstGitOid, canonicalCommitId, publicCommitId),
        ),
      ),
    ).toEqual({
      status: 409,
      body: { error: "another push already owns origin/a" },
    });
    expect(
      await projectionRequest(repository, `git/pushes/${firstBatch}/recover`, {
        incarnation,
        machineId: firstMachine,
        fence: 1,
        observations: [
          { bookmark: "a", liveOid: null },
          { bookmark: "B", liveOid: null },
        ],
      }),
    ).toEqual({
      status: 200,
      body: { pending: false, fence: 1, outcome: "aborted" },
    });
    expect((await getProjection(repository, incarnation)).body).toMatchObject({
      activationCursor: 0,
      cursors: [],
      pending: [],
    });

    const mixedBatch = "84".repeat(16);
    expect(
      await projectionRequest(
        repository,
        "git/pushes",
        projectionBatch(
          incarnation,
          mixedBatch,
          firstMachine,
          projectionUpdate("a", null, firstGitOid, canonicalCommitId, publicCommitId),
          projectionUpdate("B", null, secondGitOid, canonicalCommitId, publicCommitId),
        ),
      ),
    ).toEqual({ status: 200, body: { pending: true, fence: 2 } });
    expect(
      await projectionRequest(repository, `git/pushes/${mixedBatch}/recover`, {
        incarnation,
        machineId: firstMachine,
        fence: 2,
        observations: [
          { bookmark: "a", liveOid: firstGitOid },
          { bookmark: "B", liveOid: null },
        ],
      }),
    ).toEqual({
      status: 409,
      body: {
        error: "remote refs are mixed or ambiguous; projection batch remains quarantined",
      },
    });
    expect((await getProjection(repository, incarnation)).body).toMatchObject({
      activationCursor: 0,
      cursors: [],
      pending: [{ batchId: mixedBatch, fence: 2 }],
    });
  });

  it("requires durable commits and immutable Git receipts", async () => {
    const repository = "projection-receipts";
    await initializeProjectionRepository(repository, incarnation);
    expect(
      await projectionRequest(
        repository,
        "git/pushes",
        projectionBatch(
          incarnation,
          "80".repeat(16),
          firstMachine,
          projectionUpdate("zero", null, "00".repeat(20), "a1".repeat(64), "a2".repeat(64)),
        ),
      ),
    ).toEqual({ status: 400, body: { error: "gitOid must not be zero" } });
    expect(
      await projectionRequest(repository, "git/pushes", {
        incarnation,
        batchId: "8b".repeat(16),
        machineId: firstMachine,
        remote: "origin",
        updates: [
          {
            bookmark: "delete",
            expectedOldOid: null,
            states: [
              {
                gitOid: "97".repeat(20),
                canonicalCommitId: "a1".repeat(64),
                publicCommitId: "a2".repeat(64),
                hiddenSetId: null,
              },
            ],
            proposedState: null,
          },
        ],
      }),
    ).toEqual({
      status: 400,
      body: { error: "updates[0].states must be empty for a deletion" },
    });
    const missing = projectionBatch(
      incarnation,
      "85".repeat(16),
      firstMachine,
      projectionUpdate("main", null, "94".repeat(20), "a1".repeat(64), "a2".repeat(64)),
    );
    expect(await projectionRequest(repository, "git/pushes", missing)).toEqual({
      status: 409,
      body: { error: `canonical commit ${"a1".repeat(64)} is not cloud durable` },
    });

    const missingTreeId = "a3".repeat(64);
    const incompleteCommitId = await installObject(
      repository,
      KIND.commit,
      canonicalProjectionCommit(fromHex(missingTreeId), 9),
    );
    expect(
      await projectionRequest(
        repository,
        "git/pushes",
        projectionBatch(
          incarnation,
          "88".repeat(16),
          firstMachine,
          projectionUpdate(
            "incomplete",
            null,
            "96".repeat(20),
            incompleteCommitId,
            incompleteCommitId,
          ),
        ),
      ),
    ).toEqual({
      status: 409,
      body: {
        error: `canonical commit ${incompleteCommitId} closure is missing tree ${missingTreeId}`,
      },
    });

    const [canonicalCommitId, publicCommitId, otherPublicCommitId] =
      await installProjectionCommits(repository, 3);
    const gitOid = "95".repeat(20);
    const firstBatch = "86".repeat(16);
    expect(
      await projectionRequest(
        repository,
        "git/pushes",
        projectionBatch(
          incarnation,
          firstBatch,
          firstMachine,
          projectionUpdate("main", null, gitOid, canonicalCommitId, publicCommitId),
        ),
      ),
    ).toEqual({ status: 200, body: { pending: true, fence: 1 } });
    expect(await getProjectionReplay(repository, firstBatch, incarnation)).toMatchObject({
      status: 200,
      body: {
        updates: [
          {
            states: [
              {
                gitOid,
                canonicalCommitId,
                publicCommitId,
                hiddenSetId: null,
              },
            ],
          },
        ],
      },
    });
    expect(
      await projectionRequest(repository, `git/pushes/${firstBatch}/confirm`, {
        incarnation,
        machineId: firstMachine,
        fence: 1,
      }),
    ).toEqual({
      status: 200,
      body: { pending: false, fence: 1, outcome: "accepted" },
    });
    expect((await getProjection(repository, incarnation)).body).toMatchObject({
      cursors: [{ hiddenSetId: null }],
      mappings: [{ hiddenSetId: null }],
    });
    expect(
      await projectionRequest(
        repository,
        "git/pushes",
        projectionBatch(
          incarnation,
          "87".repeat(16),
          firstMachine,
          projectionUpdate("other", null, gitOid, canonicalCommitId, otherPublicCommitId),
        ),
      ),
    ).toEqual({
      status: 409,
      body: { error: `Git object ${gitOid} already has a different immutable receipt` },
    });
  });

  it("pages accepted mapping history under a fixed activation high-water", async () => {
    const repository = "projection-pages";
    await initializeProjectionRepository(repository, incarnation);
    const [canonicalCommitId, publicCommitId] = await installProjectionCommits(repository);
    const batchId = "89".repeat(16);
    const states = Array.from({ length: 257 }, (_, index) => ({
      gitOid: (index + 1).toString(16).padStart(40, "0"),
      canonicalCommitId,
      publicCommitId,
      hiddenSetId: null,
    }));
    expect(
      await projectionRequest(repository, "git/pushes", {
        incarnation,
        batchId,
        machineId: firstMachine,
        remote: "origin",
        updates: [
          {
            bookmark: "main",
            expectedOldOid: null,
            states,
            proposedState: 256,
          },
        ],
      }),
    ).toEqual({ status: 200, body: { pending: true, fence: 1 } });
    expect(
      await projectionRequest(repository, `git/pushes/${batchId}/confirm`, {
        incarnation,
        machineId: firstMachine,
        fence: 1,
      }),
    ).toEqual({
      status: 200,
      body: { pending: false, fence: 1, outcome: "accepted" },
    });
    const first = await getProjection(repository, incarnation);
    expect(first.status).toBe(200);
    expect(first.body).toMatchObject({
      activationCursor: 257,
      nextAfter: 256,
      through: 257,
      hasMore: true,
    });
    expect((first.body as { mappings: unknown[] }).mappings).toHaveLength(256);
    const second = await getProjection(repository, incarnation, 256, 257);
    expect(second.body).toMatchObject({ nextAfter: 257, through: 257, hasMore: false });
    expect((second.body as { mappings: unknown[] }).mappings).toHaveLength(1);
  });

  it("rejects malformed hidden-set identities before Durable Object state", async () => {
    const repository = "projection-hidden-set-id";
    await initializeProjectionRepository(repository, incarnation);
    const [canonicalCommitId, publicCommitId] = await installProjectionCommits(repository);
    const state = {
      gitOid: "a7".repeat(20),
      canonicalCommitId,
      publicCommitId,
      hiddenSetId: null as string | null,
    };
    const request = {
      incarnation,
      batchId: "a8".repeat(16),
      machineId: firstMachine,
      remote: "origin",
      updates: [
        {
          bookmark: "main",
          expectedOldOid: null,
          states: [state],
          proposedState: 0,
        },
      ],
    };
    for (const hiddenSetId of ["aa", "AA".repeat(64), "gg".repeat(64)]) {
      expect(
        await projectionRequest(repository, "git/pushes", {
          ...request,
          updates: [
            {
              ...request.updates[0],
              states: [{ ...state, hiddenSetId }],
            },
          ],
        }),
      ).toEqual({
        status: 400,
        body: {
          error: "hiddenSetId must be null or 128 lowercase hex characters",
          code: "invalid-hidden-set-id",
        },
      });
    }

    const { hiddenSetId: _, ...stateWithoutIdentity } = state;
    expect(
      await projectionRequest(repository, "git/pushes", {
        ...request,
        updates: [
          {
            ...request.updates[0],
            states: [stateWithoutIdentity],
          },
        ],
      }),
    ).toEqual({
      status: 400,
      body: {
        error:
          "request fields must be exactly gitOid, canonicalCommitId, publicCommitId, hiddenSetId",
      },
    });
  });
});

function authorizationFor(machineId: string): Record<string, string> {
  return {
    authorization: `Bearer ${env.DEVSPACE_SHARED_SECRET}`,
    "x-devspace-machine-id": machineId,
  };
}

async function ensureRepository(name: string, logicalIncarnation = "00".repeat(16)) {
  const existing = repositories.get(name);
  if (existing !== undefined) return existing;
  const response = await exports.default.fetch(
    new Request("https://example.com/repositories", {
      method: "POST",
      headers: { ...authorization, "content-type": "application/json" },
      body: JSON.stringify({ name, idempotencyKey: randomHex(16) }),
    }),
  );
  if (!response.ok) throw new Error(`failed to create test repository ${name}: ${await response.text()}`);
  const body = (await response.json()) as { repositoryId: string; incarnation: string };
  const repository = { ...body, logicalIncarnation };
  repositories.set(name, repository);
  return repository;
}

async function repositoryStub(name: string, logicalIncarnation?: string) {
  const repository = await ensureRepository(name, logicalIncarnation);
  return env.REPOSITORIES.get(env.REPOSITORIES.idFromString(repository.repositoryId));
}

async function repositoryAuthority(name: string, machineId = defaultMachine) {
  const repository = await ensureRepository(name);
  return {
    userId: env.DEVSPACE_DEVELOPMENT_USER_ID,
    machineId,
    repositoryId: repository.repositoryId,
    incarnation: repository.incarnation,
  };
}

async function repositoryRequest(
  name: string,
  path: string,
  init: RequestInit,
  machineId = defaultMachine,
) {
  const repository = await ensureRepository(name);
  return exports.default.fetch(
    new Request(`https://example.com/repositories/${repository.repositoryId}/${path}`, {
      ...init,
      headers: {
        ...authorizationFor(machineId),
        "x-devspace-incarnation": repository.incarnation,
        ...init.headers,
      },
    }),
  );
}

function translatedIncarnation(
  repository: { incarnation: string; logicalIncarnation: string },
  value: unknown,
) {
  return value === repository.logicalIncarnation ? repository.incarnation : value;
}

function randomHex(bytes: number): string {
  return Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

async function putManifest(repository: string, packId: string, bytes: Uint8Array) {
  return repositoryRequest(
    repository,
    `packs/${packId}/manifest`,
    { method: "PUT", body: bytes },
  );
}

async function initializeProjectionRepository(repository: string, incarnation: string) {
  expect(await initialize(repository, incarnation)).toEqual({
    status: 200,
    body: { initialized: true, cursor: 0, heads: [] },
  });
}

async function installProjectionCommits(repository: string, count = 2): Promise<string[]> {
  const fileId = await installObject(
    repository,
    KIND.file,
    new TextEncoder().encode("projection fixture\n"),
  );
  const treeId = await installObject(
    repository,
    KIND.tree,
    treeWithFiles([["visible.txt", fromHex(fileId)]]),
  );
  const ids: string[] = [];
  for (let index = 0; index < count; index += 1) {
    ids.push(
      await installObject(
        repository,
        KIND.commit,
        canonicalProjectionCommit(fromHex(treeId), index + 1),
      ),
    );
  }
  return ids;
}

function canonicalProjectionCommit(rootTreeId: Uint8Array, seed: number): Uint8Array {
  const signature = field(3, new Uint8Array());
  return concat(
    field(1, new Uint8Array(64)),
    field(3, rootTreeId),
    field(4, new Uint8Array(16).fill(seed)),
    field(5, new TextEncoder().encode(`projection ${seed}`)),
    field(6, signature),
    field(7, signature),
  );
}

function projectionUpdate(
  bookmark: string,
  expectedOldOid: string | null,
  gitOid: string,
  canonicalCommitId: string,
  publicCommitId: string,
  hiddenSetId: string | null = null,
) {
  return {
    bookmark,
    expectedOldOid,
    states: [{ gitOid, canonicalCommitId, publicCommitId, hiddenSetId }],
    proposedState: 0,
  };
}

function projectionBatch(
  incarnation: string,
  batchId: string,
  machineId: string,
  ...updates: ReturnType<typeof projectionUpdate>[]
) {
  return { incarnation, batchId, machineId, remote: "origin", updates };
}

async function projectionRequest(repository: string, path: string, body: unknown) {
  const record = body as { incarnation?: unknown; machineId?: unknown };
  const target = await ensureRepository(
    repository,
    typeof record.incarnation === "string" ? record.incarnation : undefined,
  );
  const translated = {
    ...(body as Record<string, unknown>),
    ...(record.incarnation === undefined
      ? {}
      : { incarnation: translatedIncarnation(target, record.incarnation) }),
  };
  const machineId = typeof record.machineId === "string" ? record.machineId : defaultMachine;
  return projectionRawRequest(
    repository,
    path,
    new TextEncoder().encode(JSON.stringify(translated)),
    machineId,
  );
}

async function projectionRawRequest(
  repository: string,
  path: string,
  body: Uint8Array,
  machineId = defaultMachine,
) {
  const response = await repositoryRequest(
    repository,
    path,
    { method: "POST", headers: { "content-type": "application/json" }, body },
    machineId,
  );
  return { status: response.status, body: await response.json() };
}

async function getProjection(
  repository: string,
  incarnation: string,
  after = 0,
  through?: number,
) {
  const highWater = through === undefined ? "" : `&through=${through}`;
  const target = await ensureRepository(repository, incarnation);
  const response = await repositoryRequest(
    repository,
    `projection?incarnation=${translatedIncarnation(target, incarnation)}&after=${after}${highWater}`,
    {},
  );
  return { status: response.status, body: await response.json() };
}

async function getProjectionReplay(repository: string, batchId: string, incarnation: string) {
  const target = await ensureRepository(repository, incarnation);
  const response = await repositoryRequest(
    repository,
    `git/pushes/${batchId}/replay?incarnation=${translatedIncarnation(target, incarnation)}`,
    {},
  );
  return { status: response.status, body: await response.json() };
}

async function putChunk(repository: string, packId: string, position: number, bytes: Uint8Array) {
  return repositoryRequest(
    repository,
    `packs/${packId}/chunks/${position}`,
    { method: "PUT", body: bytes },
  );
}

async function install(repository: string, packId: string) {
  return repositoryRequest(repository, `packs/${packId}/install`, { method: "POST" });
}

async function installOperation(
  repository: string,
  description: string,
  parents: string[] = [zeroId],
): Promise<string> {
  return installObject(
    repository,
    KIND.operation,
    canonicalOperation(new Uint8Array(64), description, parents.map(fromHex)),
  );
}

async function installObject(repository: string, kind: number, bytes: Uint8Array): Promise<string> {
  const fixture = singleObjectPack(kind, bytes);
  const manifestResponse = await putManifest(repository, fixture.id, fixture.manifest);
  expect(manifestResponse.status, JSON.stringify(await manifestResponse.clone().json())).toBe(200);
  expect((await putChunk(repository, fixture.id, 0, bytes)).status).toBe(200);
  expect((await install(repository, fixture.id)).status).toBe(200);
  return fixture.objectId;
}

function headRequest(
  incarnation: string,
  idempotencyKey: string,
  newHead: string,
  observedHeads: string[],
) {
  return { incarnation, idempotencyKey, newHead, observedHeads };
}

async function headTransaction(
  repository: string,
  incarnation: string,
  idempotencyKey: string,
  newHead: string,
  observedHeads: string[],
) {
  return postHeads(repository, headRequest(incarnation, idempotencyKey, newHead, observedHeads));
}

async function initialize(repository: string, incarnation: string) {
  const existing = repositories.get(repository);
  if (existing !== undefined) {
    if (existing.logicalIncarnation !== incarnation) {
      return { status: 409, body: { error: "repository incarnation does not match" } };
    }
    const heads = await getHeads(repository, incarnation);
    return {
      status: heads.status,
      body: heads.status === 200
        ? { initialized: false, ...(heads.body as Record<string, unknown>) }
        : heads.body,
    };
  }
  await ensureRepository(repository, incarnation);
  return { status: 200, body: { initialized: true, cursor: 0, heads: [] } };
}

async function postHeads(repository: string, body: unknown) {
  return postRawHeads(repository, JSON.stringify(body));
}

async function postRawHeads(repository: string, body: string) {
  let translated = body;
  try {
    const parsed = JSON.parse(body) as Record<string, unknown>;
    const target = await ensureRepository(
      repository,
      typeof parsed.incarnation === "string" ? parsed.incarnation : undefined,
    );
    if (parsed.incarnation !== undefined) {
      parsed.incarnation = translatedIncarnation(target, parsed.incarnation);
      translated = JSON.stringify(parsed);
    }
  } catch {
    await ensureRepository(repository);
  }
  const response = await repositoryRequest(
    repository,
    "heads",
    { method: "POST", headers: { "content-type": "application/json" }, body: translated },
  );
  return { status: response.status, body: await response.json() };
}

async function getHeads(repository: string, incarnation: string) {
  const target = await ensureRepository(repository, incarnation);
  const response = await repositoryRequest(
    repository,
    `heads?incarnation=${translatedIncarnation(target, incarnation)}`,
    {},
  );
  return { status: response.status, body: await response.json() };
}

async function objectInventory(repository: string, body: unknown) {
  return objectInventoryRaw(repository, new TextEncoder().encode(JSON.stringify(body)));
}

async function objectInventoryRaw(repository: string, body: Uint8Array) {
  let bytes = body;
  try {
    const parsed = JSON.parse(new TextDecoder().decode(body)) as Record<string, unknown>;
    const target = await ensureRepository(
      repository,
      typeof parsed.incarnation === "string" ? parsed.incarnation : undefined,
    );
    parsed.incarnation = translatedIncarnation(target, parsed.incarnation);
    bytes = new TextEncoder().encode(JSON.stringify(parsed));
  } catch {
    await ensureRepository(repository);
  }
  const response = await repositoryRequest(
    repository,
    "objects/inventory",
    { method: "POST", headers: { "content-type": "application/json" }, body: bytes },
  );
  return { status: response.status, body: await response.json() };
}

async function listPacks(
  repository: string,
  incarnation: string,
  after: number,
  through?: number,
) {
  const highWater = through === undefined ? "" : `&through=${through}`;
  const target = await ensureRepository(repository, incarnation);
  const response = await repositoryRequest(
    repository,
    `packs?incarnation=${translatedIncarnation(target, incarnation)}&after=${after}${highWater}`,
    {},
  );
  return { status: response.status, body: await response.json() };
}

async function downloadPackManifest(repository: string, packId: string, incarnation: string) {
  const target = await ensureRepository(repository, incarnation);
  const response = await repositoryRequest(
    repository,
    `packs/${packId}/manifest?incarnation=${translatedIncarnation(target, incarnation)}`,
    {},
  );
  expect(response.status).toBe(200);
  return new Uint8Array(await response.arrayBuffer());
}

async function fetchPackChunk(
  repository: string,
  packId: string,
  position: number,
  incarnation: string,
) {
  const target = await ensureRepository(repository, incarnation);
  return repositoryRequest(
    repository,
    `packs/${packId}/chunks/${position}?incarnation=${translatedIncarnation(target, incarnation)}`,
    {},
  );
}

async function downloadPackChunk(
  repository: string,
  packId: string,
  position: number,
  incarnation: string,
) {
  const response = await fetchPackChunk(repository, packId, position, incarnation);
  expect(response.status).toBe(200);
  return new Uint8Array(await response.arrayBuffer());
}

function helloManifest(): Uint8Array {
  const id = fromHex(helloId);
  const bytes = new Uint8Array(96 + 88 + 80);
  const view = new DataView(bytes.buffer);
  bytes.set(new TextEncoder().encode("DSPK"));
  view.setUint16(4, 1, true);
  view.setUint32(8, 1024 * 1024, true);
  view.setUint32(16, 1, true);
  view.setUint32(20, 1, true);
  view.setBigUint64(24, 5n, true);
  bytes.set(id, 32);
  bytes[96] = 0;
  bytes.set(id, 104);
  view.setBigUint64(176, 5n, true);
  view.setUint32(192, 5, true);
  bytes.set(id, 200);
  return bytes;
}

// Mirrors heads_manifest_matches_the_worker_protocol_vector in
// crates/machine/src/pack_manifest.rs byte for byte.
function headsManifest(): Uint8Array {
  const manifest = new Uint8Array(96 + 2 * 64 + 2 * 88 + 2 * 80);
  const view = new DataView(manifest.buffer);
  manifest.set(new TextEncoder().encode("DSPK"));
  view.setUint16(4, 1, true);
  view.setUint32(8, 64 * 1024, true);
  view.setUint32(12, 2, true);
  view.setUint32(16, 2, true);
  view.setUint32(20, 2, true);
  view.setBigUint64(24, BigInt(80 * 1024), true);
  manifest.set(new Uint8Array(64).fill(0x77), 32);
  manifest.set(new Uint8Array(64).fill(0x11), 96);
  manifest.set(new Uint8Array(64).fill(0x22), 160);
  const objects = 96 + 2 * 64;
  manifest[objects] = KIND.file;
  manifest.set(new Uint8Array(64).fill(0x33), objects + 8);
  view.setBigUint64(objects + 72, 0n, true);
  view.setBigUint64(objects + 80, BigInt(40 * 1024), true);
  manifest[objects + 88] = KIND.tree;
  manifest.set(new Uint8Array(64).fill(0x44), objects + 96);
  view.setBigUint64(objects + 160, BigInt(40 * 1024), true);
  view.setBigUint64(objects + 168, BigInt(40 * 1024), true);
  const chunks = objects + 2 * 88;
  view.setBigUint64(chunks, 0n, true);
  view.setUint32(chunks + 8, 64 * 1024, true);
  manifest.set(new Uint8Array(64).fill(0x55), chunks + 16);
  view.setBigUint64(chunks + 80, BigInt(64 * 1024), true);
  view.setUint32(chunks + 88, 16 * 1024, true);
  manifest.set(new Uint8Array(64).fill(0x66), chunks + 96);
  return manifest;
}

function singleObjectPack(kind: number, objectBytes: Uint8Array) {
  const kernel = new Kernel();
  const objectId = kernel.validate(kind, objectBytes).id;
  const packHash = kernel.hash([objectBytes]);
  const manifest = new Uint8Array(96 + 88 + 80);
  const view = new DataView(manifest.buffer);
  manifest.set(new TextEncoder().encode("DSPK"));
  view.setUint16(4, 1, true);
  view.setUint32(8, 1024 * 1024, true);
  view.setUint32(16, 1, true);
  view.setUint32(20, 1, true);
  view.setBigUint64(24, BigInt(objectBytes.byteLength), true);
  manifest.set(packHash, 32);
  manifest[96] = kind;
  manifest.set(objectId, 104);
  view.setBigUint64(176, BigInt(objectBytes.byteLength), true);
  view.setUint32(192, objectBytes.byteLength, true);
  manifest.set(packHash, 200);
  return { id: toHex(kernel.hash([manifest])), manifest, objectId: toHex(objectId) };
}

function rollbackFixture() {
  const kernel = new Kernel();
  const first = new TextEncoder().encode("a");
  const second = new TextEncoder().encode("b");
  const bytes = concat(first, second);
  const firstId = kernel.validate(0, first).id;
  const secondDeclaredId = new Uint8Array(64).fill(0xff);
  const packHash = kernel.hash([bytes]);
  const manifest = new Uint8Array(96 + 2 * 88 + 80);
  const view = new DataView(manifest.buffer);
  manifest.set(new TextEncoder().encode("DSPK"));
  view.setUint16(4, 1, true);
  view.setUint32(8, 1024 * 1024, true);
  view.setUint32(16, 2, true);
  view.setUint32(20, 1, true);
  view.setBigUint64(24, 2n, true);
  manifest.set(packHash, 32);
  manifest[96] = 0;
  manifest.set(firstId, 104);
  view.setBigUint64(176, 1n, true);
  manifest[184] = 0;
  manifest.set(secondDeclaredId, 192);
  view.setBigUint64(256, 1n, true);
  view.setBigUint64(264, 1n, true);
  view.setUint32(280, 2, true);
  manifest.set(packHash, 288);
  return { bytes, id: toHex(kernel.hash([manifest])), manifest };
}

function spanningObjectPack() {
  const kernel = new Kernel();
  const chunkBytes = 64 * 1024;
  const objects = [new Uint8Array(40 * 1024).fill(1), new Uint8Array(40 * 1024).fill(2)]
    .map((bytes) => ({ bytes, id: kernel.validate(KIND.file, bytes).id }))
    .sort((left, right) => toHex(left.id).localeCompare(toHex(right.id)));
  const packBytes = concat(...objects.map((object) => object.bytes));
  const chunks = [packBytes.slice(0, chunkBytes), packBytes.slice(chunkBytes)];
  const packHash = kernel.hash([packBytes]);
  const manifest = new Uint8Array(96 + objects.length * 88 + chunks.length * 80);
  const view = new DataView(manifest.buffer);
  manifest.set(new TextEncoder().encode("DSPK"));
  view.setUint16(4, 1, true);
  view.setUint32(8, chunkBytes, true);
  view.setUint32(16, objects.length, true);
  view.setUint32(20, chunks.length, true);
  view.setBigUint64(24, BigInt(packBytes.byteLength), true);
  manifest.set(packHash, 32);
  let byteOffset = 0;
  for (const [position, object] of objects.entries()) {
    const offset = 96 + position * 88;
    manifest[offset] = KIND.file;
    manifest.set(object.id, offset + 8);
    view.setBigUint64(offset + 72, BigInt(byteOffset), true);
    view.setBigUint64(offset + 80, BigInt(object.bytes.byteLength), true);
    byteOffset += object.bytes.byteLength;
  }
  byteOffset = 0;
  for (const [position, chunk] of chunks.entries()) {
    const offset = 96 + objects.length * 88 + position * 80;
    view.setBigUint64(offset, BigInt(byteOffset), true);
    view.setUint32(offset + 8, chunk.byteLength, true);
    manifest.set(kernel.hash([chunk]), offset + 16);
    byteOffset += chunk.byteLength;
  }
  return { id: toHex(kernel.hash([manifest])), manifest, chunks };
}

function zeroLengthMiddleObjectPack() {
  const kernel = new Kernel();
  const objects = [
    { kind: KIND.file, bytes: new TextEncoder().encode("before") },
    { kind: KIND.tree, bytes: new Uint8Array() },
    { kind: KIND.view, bytes: canonicalEmptyView() },
  ].map((object) => ({ ...object, id: kernel.validate(object.kind, object.bytes).id }));
  const chunk = concat(...objects.map((object) => object.bytes));
  const packHash = kernel.hash([chunk]);
  const manifest = new Uint8Array(96 + objects.length * 88 + 80);
  const view = new DataView(manifest.buffer);
  manifest.set(new TextEncoder().encode("DSPK"));
  view.setUint16(4, 1, true);
  view.setUint32(8, 64 * 1024, true);
  view.setUint32(16, objects.length, true);
  view.setUint32(20, 1, true);
  view.setBigUint64(24, BigInt(chunk.byteLength), true);
  manifest.set(packHash, 32);
  let byteOffset = 0;
  for (const [position, object] of objects.entries()) {
    const offset = 96 + position * 88;
    manifest[offset] = object.kind;
    manifest.set(object.id, offset + 8);
    view.setBigUint64(offset + 72, BigInt(byteOffset), true);
    view.setBigUint64(offset + 80, BigInt(object.bytes.byteLength), true);
    byteOffset += object.bytes.byteLength;
  }
  const chunkOffset = 96 + objects.length * 88;
  view.setUint32(chunkOffset + 8, chunk.byteLength, true);
  manifest.set(packHash, chunkOffset + 16);
  return { id: toHex(kernel.hash([manifest])), manifest, chunk };
}

function fromHex(value: string): Uint8Array {
  return Uint8Array.from({ length: value.length / 2 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

function treeWithFiles(entries: Array<[string, Uint8Array]>): Uint8Array {
  return concat(
    ...entries.map(([name, id]) => {
      const file = field(1, id);
      const value = field(2, file);
      const entry = concat(field(1, new TextEncoder().encode(name)), field(2, value));
      return field(1, entry);
    }),
  );
}

function canonicalEmptyView(): Uint8Array {
  return new Uint8Array([0x4a, 0x04, 0x1a, 0x02, 0x12, 0x00, 0x60, 0x01]);
}

function canonicalOperation(
  viewId: Uint8Array,
  description: string,
  parents: Uint8Array[] = [new Uint8Array(64)],
): Uint8Array {
  const metadata = concat(
    field(1, new Uint8Array()),
    field(2, new Uint8Array()),
    field(3, new TextEncoder().encode(description)),
  );
  return concat(
    field(1, viewId),
    ...parents.map((parent) => field(2, parent)),
    field(3, metadata),
  );
}

function field(number: number, bytes: Uint8Array): Uint8Array {
  if (bytes.byteLength >= 128) throw new Error("test protobuf helper only supports short fields");
  return concat(new Uint8Array([(number << 3) | 2, bytes.byteLength]), bytes);
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const output = new Uint8Array(parts.reduce((length, part) => length + part.byteLength, 0));
  let offset = 0;
  for (const part of parts) {
    output.set(part, offset);
    offset += part.byteLength;
  }
  return output;
}

function decodeRle(value: string): Uint8Array {
  const output: number[] = [];
  for (const run of value.split(",")) {
    const [byte, count] = run.split("*");
    output.push(...Array(Number(count)).fill(Number.parseInt(byte, 16)));
  }
  return new Uint8Array(output);
}
