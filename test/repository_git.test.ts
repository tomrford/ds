import { env, exports } from "cloudflare:workers";
import { evictDurableObject } from "cloudflare:test";
import { beforeAll, describe, expect, it } from "vitest";
import gitGolden from "../crates/kernel-git/tests/git_golden.txt?raw";
import { KernelGit, gitToHex } from "../src/kernel_git";
import {
  MAX_GIT_CHUNK_BYTES,
  MAX_GIT_MANIFEST_BYTES,
  decodeGitManifest,
} from "../src/pack_git_protocol";
import fixtures from "./fixtures/repository_git.json";

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

    const kernel = new KernelGit();
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
    expect((await routeRequest("git-bounds", "git/packs/bad/manifest", { method: "PUT" })).status).toBe(
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
  return env.REPOSITORIES_GIT.getByName(repository.repositoryId);
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
