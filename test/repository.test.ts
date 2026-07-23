import { env, exports } from "cloudflare:workers";
import { evictDurableObject } from "cloudflare:test";
import { beforeAll, describe, expect, it } from "vitest";
import gitGolden from "../crates/kernel/tests/git_golden.txt?raw";
import { Kernel, gitToHex } from "../src/kernel";
import {
  MAX_GIT_CHUNK_BYTES,
  MAX_GIT_MANIFEST_BYTES,
  decodeGitManifest,
} from "../src/pack_protocol";
import fixtures from "./fixtures/repository.json";

const defaultMachine = "a6".repeat(16);
const repositories = new Map<string, { repositoryId: string; incarnation: string }>();
let authorization: Record<string, string>;

beforeAll(() => {
  authorization = authorizationFor(defaultMachine);
});

describe("Git v2 manifest and validation kernel", () => {
  it("parses Rust-generated v2 manifests and rejects malformed real Git vectors", () => {
    const manifest = decodeGitManifest(decodeHex(fixtures.complete.manifest));
    expect(manifest).toMatchObject({
      chunkBytes: 64 * 1024,
      headCommits: [expect.any(Uint8Array)],
      objects: [
        { kind: 0, id: expect.any(Uint8Array) },
        { kind: 1, id: expect.any(Uint8Array) },
        { kind: 2, id: expect.any(Uint8Array) },
      ],
    });
    expect(manifest.headCommits[0]).toHaveLength(20);
    expect(manifest.objects.every((object) => object.id.byteLength === 20)).toBe(true);

    const lines = gitGolden
      .split("\n")
      .filter((line) => line.startsWith("tree|") || line.startsWith("commit|"));
    const tree = decodeHex(
      lines.find((line) => line.startsWith("tree|") && line.split("|")[2] !== "")!.split("|")[2],
    );
    const commit = decodeHex(lines.find((line) => line.startsWith("commit|"))!.split("|")[2]);
    const headerTerminator = findSequence(commit, new Uint8Array([0x0a, 0x0a]));
    const mutatedCommit = commit.slice();
    mutatedCommit[0] = 0;

    const kernel = new Kernel();
    expect(() => kernel.validate(1, tree.slice(0, -1))).toThrow();
    expect(() => kernel.validate(2, commit.slice(0, headerTerminator + 1))).toThrow();
    expect(() => kernel.validate(2, mutatedCommit)).toThrow();

    const noncanonicalManifest = decodeHex(fixtures.complete.manifest);
    noncanonicalManifest[6] = 1;
    expect(() => decodeGitManifest(noncanonicalManifest)).toThrow("reserved bytes must be zero");
  });
});

describe("Git v2 repository object store", () => {
  it("quarantines, retries and atomically installs a pack idempotently", async () => {
    const fixture = decodedFixture(fixtures.complete);
    expect(await json(await putManifest("git-install", fixture))).toEqual({
      inserted: true,
      installed: false,
    });

    const earlyInstall = await install("git-install", fixture.id);
    expect(earlyInstall.status).toBe(400);
    expect(await earlyInstall.json()).toMatchObject({ error: "pack is missing 1 chunks" });

    expect(await json(await putChunk("git-install", fixture, 0))).toEqual({
      inserted: true,
      installed: false,
    });
    expect(await json(await putChunk("git-install", fixture, 0))).toEqual({
      inserted: false,
      installed: false,
    });
    expect(await json(await install("git-install", fixture.id))).toEqual({
      installed: true,
      insertedObjects: 3,
    });

    const stub = await repositoryGitStub("git-install");
    expect(await stub.countObjects()).toBe(3);
    expect(await stub.countObjectReferences()).toBe(2);
    expect(await stub.countInstalledPacks()).toBe(1);
    expect(await stub.countQuarantinedPacks()).toBe(0);
    expect(await json(await install("git-install", fixture.id))).toEqual({
      installed: false,
      insertedObjects: 0,
    });
    expect(await json(await putManifest("git-install", fixture))).toEqual({
      inserted: false,
      installed: true,
    });
    expect(await json(await putChunk("git-install", fixture, 0))).toEqual({
      inserted: false,
      installed: true,
    });

    expect(await json(await listPacks("git-install", 0))).toEqual({
      packs: [{ sequence: 1, id: fixture.id }],
      nextAfter: 1,
      through: 1,
      hasMore: false,
    });
    expect(await downloadManifest("git-install", fixture.id)).toEqual(fixture.manifest);
    expect(await downloadChunk("git-install", fixture.id, 0)).toEqual(fixture.chunks[0]);
  });

  it("pages the installed-pack catalog under one fixed high-water", async () => {
    const name = "git-catalog-paging";
    const authority = await repositoryAuthority(name);
    const stub = await repositoryGitStub(name);
    expect(await stub.initializeRepository(authority)).toMatchObject({ ok: true });
    const fixtures = Array.from({ length: 257 }, (_, index) => blobFixture(index));
    for (const fixture of fixtures) {
      expect(await stub.putPackManifest(authority, fixture.id, fixture.manifest)).toMatchObject({
        ok: true,
      });
      expect(await stub.putPackChunk(authority, fixture.id, 0, fixture.chunks[0])).toMatchObject({
        ok: true,
      });
      expect(await stub.installPack(authority, fixture.id)).toMatchObject({ ok: true });
    }

    const first = (await json(await listPacks(name, 0))) as {
      packs: Array<{ sequence: number; id: string }>;
      nextAfter: number;
      through: number;
      hasMore: boolean;
    };
    expect(first.packs).toHaveLength(256);
    expect(first.nextAfter).toBe(256);
    expect(first.through).toBe(257);
    expect(first.hasMore).toBe(true);

    const second = await json(await listPacks(name, first.nextAfter, first.through));
    expect(second).toEqual({
      packs: [{ sequence: 257, id: fixtures[256].id }],
      nextAfter: 257,
      through: 257,
      hasMore: false,
    });
  });

  it("rejects an incomplete closure, then installs it after the dependency arrives", async () => {
    const missing = decodedFixture(fixtures.missingReference);
    await putManifest("git-closure", missing);
    await putChunk("git-closure", missing, 0);
    const rejected = await install("git-closure", missing.id);
    expect(rejected.status).toBe(400);
    expect(await rejected.json()).toMatchObject({
      error: expect.stringContaining("missing referenced object"),
    });

    const stub = await repositoryGitStub("git-closure");
    expect(await stub.countObjects()).toBe(0);
    expect(await stub.countInstalledPacks()).toBe(0);
    expect(await stub.countQuarantinedPacks()).toBe(1);

    const dependency = decodedFixture(fixtures.dependency);
    await putManifest("git-closure", dependency);
    await putChunk("git-closure", dependency, 0);
    expect(await json(await install("git-closure", dependency.id))).toEqual({
      installed: true,
      insertedObjects: 1,
    });
    expect(await json(await install("git-closure", missing.id))).toEqual({
      installed: true,
      insertedObjects: 2,
    });
    expect(await stub.countObjects()).toBe(3);
    expect(await stub.countInstalledPacks()).toBe(2);
    expect(await stub.countQuarantinedPacks()).toBe(0);
  });

  it("rolls back earlier inserts when a later golden-derived object is malformed", async () => {
    const fixture = decodedFixture(fixtures.malformed);
    await putManifest("git-malformed", fixture);
    await putChunk("git-malformed", fixture, 0);
    const rejected = await install("git-malformed", fixture.id);
    expect(rejected.status).toBe(400);
    expect(await rejected.json()).toMatchObject({ error: expect.stringContaining("object 1 is invalid") });

    const stub = await repositoryGitStub("git-malformed");
    expect(await stub.countObjects()).toBe(0);
    expect(await stub.countObjectReferences()).toBe(0);
    expect(await stub.countInstalledPacks()).toBe(0);
    expect(await stub.countQuarantinedPacks()).toBe(1);
  });

  it("enforces authentication, authority rechecks, identifiers and body bounds", async () => {
    const repository = await ensureRepository("git-bounds");
    const fixture = decodedFixture(fixtures.complete);
    const base = `https://example.com/repositories/${repository.repositoryId}/git/packs/${fixture.id}`;
    expect(
      (
        await exports.default.fetch(
          new Request(`${base}/manifest`, { method: "PUT", body: fixture.manifest }),
        )
      ).status,
    ).toBe(401);
    expect(
      (
        await exports.default.fetch(
          new Request(`${base}/manifest`, {
            method: "PUT",
            headers: {
              authorization: `Bearer ${env.DEVSPACE_SHARED_SECRET}`,
              "x-devspace-machine-id": "invalid",
              "x-devspace-incarnation": repository.incarnation,
            },
            body: fixture.manifest,
          }),
        )
      ).status,
    ).toBe(400);

    const stale = await exports.default.fetch(
      new Request(`${base}/manifest`, {
        method: "PUT",
        headers: { ...authorization, "x-devspace-incarnation": "00".repeat(16) },
        body: fixture.manifest,
      }),
    );
    expect(stale.status).toBe(404);
    const catalogBase = `https://example.com/repositories/${repository.repositoryId}/git/packs`;
    expect((await exports.default.fetch(new Request(catalogBase))).status).toBe(401);
    expect(
      (
        await exports.default.fetch(
          new Request(`${base}/manifest`, {
            headers: { ...authorization, "x-devspace-incarnation": "00".repeat(16) },
          }),
        )
      ).status,
    ).toBe(404);
    expect((await routeRequest("git-bounds", "git/packs/bad/manifest", { method: "PUT" })).status).toBe(
      400,
    );
    expect((await routeRequest("git-bounds", "git/packs?after=-1", { method: "GET" })).status).toBe(
      400,
    );
    expect((await routeRequest("git-bounds", "git/packs?through=1", { method: "GET" })).status).toBe(
      400,
    );

    await putManifest("git-bounds", fixture);
    const stub = await repositoryGitStub("git-bounds");
    const authority = await repositoryAuthority("git-bounds");
    expect(
      await stub.putPackManifest(
        { ...authority, userId: "other-user" },
        fixture.id,
        fixture.manifest,
      ),
    ).toMatchObject({ ok: false, status: 409, code: "repository-authority-stale" });

    const oversizedManifest = await routeRequest(
      "git-bounds",
      `git/packs/${fixture.id}/manifest`,
      { method: "PUT", body: new Uint8Array(MAX_GIT_MANIFEST_BYTES + 1) },
    );
    expect(oversizedManifest.status).toBe(400);
    expect(await oversizedManifest.json()).toEqual({
      error: `Git manifest exceeds ${MAX_GIT_MANIFEST_BYTES} byte limit`,
    });
    const oversizedChunk = await routeRequest(
      "git-bounds",
      `git/packs/${fixture.id}/chunks/0`,
      { method: "PUT", body: new Uint8Array(MAX_GIT_CHUNK_BYTES + 1) },
    );
    expect(oversizedChunk.status).toBe(400);
    expect(await oversizedChunk.json()).toEqual({
      error: `Git chunk exceeds ${MAX_GIT_CHUNK_BYTES} byte limit`,
    });

    const missing = "ff".repeat(64);
    const missingManifest = await routeRequest(
      "git-bounds",
      `git/packs/${missing}/manifest`,
      { method: "GET" },
    );
    expect(missingManifest.status).toBe(404);
    expect(await missingManifest.json()).toEqual({ error: "installed pack does not exist" });
    const missingChunk = await routeRequest(
      "git-bounds",
      `git/packs/${missing}/chunks/0`,
      { method: "GET" },
    );
    expect(missingChunk.status).toBe(404);
    expect(await missingChunk.json()).toEqual({ error: "installed pack chunk does not exist" });
  });

  it("persists quarantine and installed objects across Durable Object eviction", async () => {
    const fixture = decodedFixture(fixtures.complete);
    await putManifest("git-eviction", fixture);
    await putChunk("git-eviction", fixture, 0);
    await evictDurableObject(await repositoryGitStub("git-eviction"));

    expect(await json(await install("git-eviction", fixture.id))).toEqual({
      installed: true,
      insertedObjects: 3,
    });
    await evictDurableObject(await repositoryGitStub("git-eviction"));

    const reloaded = await repositoryGitStub("git-eviction");
    expect(await reloaded.countObjects()).toBe(3);
    expect(await reloaded.countInstalledPacks()).toBe(1);
    expect(await json(await putManifest("git-eviction", fixture))).toEqual({
      inserted: false,
      installed: true,
    });
  });
});

describe("Git v2 operation store and heads", () => {
  it("rejects noncanonical objects and converges concurrent heads idempotently", async () => {
    const name = "git-ops-convergence";
    const kernel = new Kernel();
    const view = canonicalGitView();
    const viewId = gitToHex(kernel.validateView(view).id);
    expect(await json(await putOp(name, "views", viewId, view))).toEqual({ inserted: true });
    expect(await json(await putOp(name, "views", viewId, view))).toEqual({ inserted: false });

    const noncanonical = Uint8Array.from([...view, 0x68, 0x01]);
    const rejected = await putOp(name, "views", viewId, noncanonical);
    expect(rejected.status).toBe(400);
    expect(await rejected.json()).toMatchObject({
      error: expect.stringContaining("does not exactly re-encode"),
    });

    const base = await installOperation(name, viewId, "base");
    const left = await installOperation(name, viewId, "left", [base]);
    const right = await installOperation(name, viewId, "right", [base]);
    const merged = await installOperation(name, viewId, "merged", [left, right]);
    const incarnation = (await ensureRepository(name)).incarnation;

    expect(await postOpHeads(name, incarnation, "01".repeat(16), base, [])).toEqual({
      cursor: 1,
      heads: [base],
    });
    const leftRequest = opHeadRequest(incarnation, "02".repeat(16), left, [base]);
    expect(await json(await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(leftRequest),
    }))).toEqual({ cursor: 2, heads: [left] });
    expect(await postOpHeads(name, incarnation, "03".repeat(16), right, [base])).toEqual({
      cursor: 3,
      heads: [left, right].sort(),
    });
    expect(await json(await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(leftRequest),
    }))).toEqual({ cursor: 2, heads: [left] });
    expect(await postOpHeads(name, incarnation, "04".repeat(16), merged, [right, left])).toEqual({
      cursor: 4,
      heads: [merged],
    });
  });

  it("does not consume a head receipt until the complete op closure exists", async () => {
    const name = "git-ops-incomplete";
    const kernel = new Kernel();
    const view = canonicalGitView();
    const viewId = gitToHex(kernel.validateView(view).id);
    const operation = await installOperation(name, viewId, "needs view");
    const incarnation = (await ensureRepository(name)).incarnation;
    const request = opHeadRequest(incarnation, "0a".repeat(16), operation, []);

    const incomplete = await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(request),
    });
    expect(incomplete.status).toBe(409);
    expect(await incomplete.json()).toMatchObject({ code: "head-closure-incomplete" });

    expect(await json(await putOp(name, "views", viewId, view))).toEqual({ inserted: true });
    expect(await json(await routeRequest(name, "git/ops/heads/transactions", {
      method: "POST",
      body: JSON.stringify(request),
    }))).toEqual({ cursor: 1, heads: [operation] });
  });

  it("persists operation objects, exact bytes, heads and receipts across eviction", async () => {
    const name = "git-ops-eviction";
    const kernel = new Kernel();
    const view = canonicalGitView();
    const viewId = gitToHex(kernel.validateView(view).id);
    await putOp(name, "views", viewId, view);
    const operation = await installOperation(name, viewId, "persistent");
    const incarnation = (await ensureRepository(name)).incarnation;
    expect(await postOpHeads(name, incarnation, "11".repeat(16), operation, [])).toEqual({
      cursor: 1,
      heads: [operation],
    });

    await evictDurableObject(await repositoryGitStub(name));
    const downloaded = await routeRequest(name, `git/ops/operations/${operation}`, {
      method: "GET",
    });
    expect(downloaded.status).toBe(200);
    expect(new Uint8Array(await downloaded.arrayBuffer())).toEqual(
      canonicalGitOperation(decodeHex(viewId), "persistent"),
    );
    expect(await json(await routeRequest(name, "git/ops/heads", { method: "GET" }))).toEqual({
      cursor: 1,
      heads: [operation],
    });
    expect(await postOpHeads(name, incarnation, "11".repeat(16), operation, [])).toEqual({
      cursor: 1,
      heads: [operation],
    });
    expect(await (await repositoryGitStub(name)).countOpObjects()).toBe(2);
  });
});

interface EncodedFixture {
  id: string;
  manifest: string;
  chunks: string[];
}

interface DecodedFixture {
  id: string;
  manifest: Uint8Array;
  chunks: Uint8Array[];
}

function decodedFixture(fixture: EncodedFixture): DecodedFixture {
  return {
    id: fixture.id,
    manifest: decodeHex(fixture.manifest),
    chunks: fixture.chunks.map(decodeHex),
  };
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

async function repositoryAuthority(name: string) {
  const repository = await ensureRepository(name);
  return {
    userId: env.DEVSPACE_DEVELOPMENT_USER_ID,
    machineId: defaultMachine,
    repositoryId: repository.repositoryId,
    incarnation: repository.incarnation,
  };
}

async function repositoryGitStub(name: string) {
  const repository = await ensureRepository(name);
  return env.REPOSITORIES.getByName(repository.repositoryId);
}

async function routeRequest(name: string, path: string, init: RequestInit) {
  const repository = await ensureRepository(name);
  return exports.default.fetch(
    new Request(`https://example.com/repositories/${repository.repositoryId}/${path}`, {
      ...init,
      headers: {
        ...authorization,
        "x-devspace-incarnation": repository.incarnation,
        ...init.headers,
      },
    }),
  );
}

function putManifest(name: string, fixture: DecodedFixture) {
  return routeRequest(name, `git/packs/${fixture.id}/manifest`, {
    method: "PUT",
    body: fixture.manifest,
  });
}

function putChunk(name: string, fixture: DecodedFixture, position: number) {
  return routeRequest(name, `git/packs/${fixture.id}/chunks/${position}`, {
    method: "PUT",
    body: fixture.chunks[position],
  });
}

function install(name: string, packId: string) {
  return routeRequest(name, `git/packs/${packId}/install`, { method: "POST" });
}

function putOp(
  name: string,
  kind: "views" | "operations",
  id: string,
  bytes: Uint8Array,
) {
  return routeRequest(name, `git/ops/${kind}/${id}`, { method: "PUT", body: bytes });
}

async function installOperation(
  name: string,
  viewId: string,
  description: string,
  parents = ["00".repeat(64)],
): Promise<string> {
  const bytes = canonicalGitOperation(decodeHex(viewId), description, parents.map(decodeHex));
  const id = gitToHex(new Kernel().validateOperation(bytes).id);
  expect(await json(await putOp(name, "operations", id, bytes))).toMatchObject({
    inserted: expect.any(Boolean),
  });
  return id;
}

async function postOpHeads(
  name: string,
  incarnation: string,
  idempotencyKey: string,
  newHead: string,
  observedHeads: string[],
) {
  return json(await routeRequest(name, "git/ops/heads/transactions", {
    method: "POST",
    body: JSON.stringify(opHeadRequest(incarnation, idempotencyKey, newHead, observedHeads)),
  }));
}

function opHeadRequest(
  incarnation: string,
  idempotencyKey: string,
  newHead: string,
  observedHeads: string[],
) {
  return { incarnation, idempotencyKey, newHead, observedHeads };
}

function canonicalGitView(): Uint8Array {
  return new Uint8Array([
    0x0a,
    20,
    ...new Uint8Array(20),
    0x4a,
    4,
    0x1a,
    2,
    0x12,
    0,
    0x60,
    1,
  ]);
}

function canonicalGitOperation(
  viewId: Uint8Array,
  description: string,
  parents: Uint8Array[] = [new Uint8Array(64)],
): Uint8Array {
  const metadata: number[] = [0x0a, 0, 0x12, 0];
  pushProtoBytes(metadata, 3, new TextEncoder().encode(description));
  const operation: number[] = [];
  pushProtoBytes(operation, 1, viewId);
  for (const parent of parents) pushProtoBytes(operation, 2, parent);
  pushProtoBytes(operation, 3, Uint8Array.from(metadata));
  return Uint8Array.from(operation);
}

function pushProtoBytes(output: number[], tag: number, bytes: Uint8Array) {
  output.push((tag << 3) | 2);
  let length = bytes.byteLength;
  while (length >= 0x80) {
    output.push((length & 0x7f) | 0x80);
    length >>= 7;
  }
  output.push(length);
  output.push(...bytes);
}

function listPacks(name: string, after: number, through?: number) {
  const highWater = through === undefined ? "" : `&through=${through}`;
  return routeRequest(name, `git/packs?after=${after}${highWater}`, { method: "GET" });
}

async function downloadManifest(name: string, packId: string): Promise<Uint8Array> {
  const response = await routeRequest(name, `git/packs/${packId}/manifest`, { method: "GET" });
  expect(response.status).toBe(200);
  expect(response.headers.get("content-type")).toBe("application/octet-stream");
  return new Uint8Array(await response.arrayBuffer());
}

async function downloadChunk(name: string, packId: string, position: number): Promise<Uint8Array> {
  const response = await routeRequest(name, `git/packs/${packId}/chunks/${position}`, {
    method: "GET",
  });
  expect(response.status).toBe(200);
  expect(response.headers.get("content-type")).toBe("application/octet-stream");
  return new Uint8Array(await response.arrayBuffer());
}

function blobFixture(index: number): DecodedFixture {
  const bytes = new TextEncoder().encode(`catalog fixture ${index}\n`);
  const kernel = new Kernel();
  const objectId = kernel.validate(0, bytes).id;
  const hash = kernel.hash([bytes]);
  const manifest = new Uint8Array(96 + 44 + 80);
  const view = new DataView(manifest.buffer);
  manifest.set([0x44, 0x53, 0x50, 0x4b]);
  view.setUint16(4, 2, true);
  view.setUint32(8, 64 * 1024, true);
  view.setUint32(16, 1, true);
  view.setUint32(20, 1, true);
  view.setBigUint64(24, BigInt(bytes.byteLength), true);
  manifest.set(hash, 32);
  manifest[96] = 0;
  manifest.set(objectId, 104);
  view.setBigUint64(124, 0n, true);
  view.setBigUint64(132, BigInt(bytes.byteLength), true);
  view.setBigUint64(140, 0n, true);
  view.setUint32(148, bytes.byteLength, true);
  manifest.set(hash, 156);
  return { id: gitToHex(kernel.hash([manifest])), manifest, chunks: [bytes] };
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
  if (value.length % 2 !== 0) throw new Error("odd-length hex fixture");
  return Uint8Array.from({ length: value.length / 2 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

function findSequence(bytes: Uint8Array, sequence: Uint8Array): number {
  for (let index = 0; index <= bytes.byteLength - sequence.byteLength; index += 1) {
    if (sequence.every((byte, offset) => bytes[index + offset] === byte)) return index;
  }
  throw new Error(`sequence not found in ${gitToHex(bytes)}`);
}

async function json(response: Response): Promise<unknown> {
  expect(response.status).toBe(200);
  return response.json();
}
