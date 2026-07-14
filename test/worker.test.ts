import { env, exports } from "cloudflare:workers";
import { evictDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import v2Golden from "../crates/kernel/tests/v2_golden.txt?raw";
import { KIND, Kernel, isKindName, toHex } from "../src/kernel";
import { MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, decodeManifest } from "../src/pack_protocol";

const authorization = { authorization: "Bearer test-token" };
const helloId =
  "e4cfa39a3d37be31c59609e807970799caa68a19bfaa15135f165085e01d41a65ba1e1b146aeb6bd0092b49eac214c103ccfa3a365954bbbe52f74a2b3620c94";
const helloPackId =
  "606591ef0c95a0b8ab99b4ccc8cfd34f05e143f82cf4e7ff0766183d21f0fce42456f1d602deaaef70fcaed78de2ca8cee73a055853d7aff1409c7a26b185733";
const hello = new TextEncoder().encode("hello");

describe("validation kernel", () => {
  it("matches v2 IDs for every object kind through Wasm", () => {
    const kernel = new Kernel();
    for (const line of v2Golden.trim().split("\n")) {
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
    const stub = env.REPOSITORIES.getByName("pack");
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
    const stub = env.REPOSITORIES.getByName("eviction");
    await stub.putPackManifest(helloPackId, helloManifest());
    await stub.putPackChunk(helloPackId, 0, hello);
    await evictDurableObject(stub);

    const reloaded = env.REPOSITORIES.getByName("eviction");
    expect(await reloaded.installPack(helloPackId)).toEqual({
      ok: true,
      installed: true,
      insertedObjects: 1,
    });
    expect(await reloaded.countObjects()).toBe(1);
    await evictDurableObject(reloaded);
    expect(await env.REPOSITORIES.getByName("eviction").putPackManifest(helloPackId, helloManifest()))
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
    expect(await env.REPOSITORIES.getByName("cross-pack").countObjects()).toBe(1);
  });

  it("rolls back earlier object inserts when a later object is invalid", async () => {
    const fixture = rollbackFixture();
    expect((await putManifest("rollback", fixture.id, fixture.manifest)).status).toBe(200);
    expect((await putChunk("rollback", fixture.id, 0, fixture.bytes)).status).toBe(200);
    const response = await install("rollback", fixture.id);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: "object 1 ID does not match manifest" });
    const stub = env.REPOSITORIES.getByName("rollback");
    expect(await stub.countObjects()).toBe(0);
    expect(await stub.countInstalledPacks()).toBe(0);
  });

  it("rejects noncanonical manifests before quarantine", async () => {
    const manifest = helloManifest();
    manifest[6] = 1;
    expect(() => decodeManifest(manifest)).toThrow("reserved bytes must be zero");
    const response = await putManifest("bad-manifest", helloPackId, manifest);
    expect(response.status).toBe(400);
    expect(await response.json()).toMatchObject({ error: expect.stringContaining("hashes to") });
    expect(await env.REPOSITORIES.getByName("bad-manifest").countInstalledPacks()).toBe(0);
  });

  it("enforces authentication, identifiers, routes and body bounds", async () => {
    const unauthorized = await exports.default.fetch(
      new Request(`https://example.com/repositories/repo/packs/${helloPackId}/manifest`, {
        method: "PUT",
        body: helloManifest(),
      }),
    );
    expect(unauthorized.status).toBe(401);
    expect((await putManifest("UPPER", helloPackId, helloManifest())).status).toBe(400);
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

async function putManifest(repository: string, packId: string, bytes: Uint8Array) {
  return exports.default.fetch(
    new Request(`https://example.com/repositories/${repository}/packs/${packId}/manifest`, {
      method: "PUT",
      headers: authorization,
      body: bytes,
    }),
  );
}

async function putChunk(repository: string, packId: string, position: number, bytes: Uint8Array) {
  return exports.default.fetch(
    new Request(
      `https://example.com/repositories/${repository}/packs/${packId}/chunks/${position}`,
      { method: "PUT", headers: authorization, body: bytes },
    ),
  );
}

async function install(repository: string, packId: string) {
  return exports.default.fetch(
    new Request(`https://example.com/repositories/${repository}/packs/${packId}/install`, {
      method: "POST",
      headers: authorization,
    }),
  );
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
  return { id: toHex(kernel.hash([manifest])), manifest };
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
