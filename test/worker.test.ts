import { env, exports } from "cloudflare:workers";
import { evictDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import v2Golden from "../crates/kernel/tests/v2_golden.txt?raw";

const authorization = { authorization: "Bearer test-token" };

describe("validation Durable Object", () => {
  it("matches v2 IDs for every object kind through Wasm", async () => {
    for (const line of v2Golden.trim().split("\n")) {
      const [kind, expectedId, encoded] = line.split("|");
      const response = await put(`golden-${kind}`, kind, decodeRle(encoded));
      expect(response.status, `${kind} response`).toBe(200);
      expect(await response.json(), `${kind} ID`).toMatchObject({ id: expectedId });
    }
  });

  it("runs the Wasm kernel and stores canonical bytes idempotently", async () => {
    const first = await put("repo", "file", new TextEncoder().encode("hello"));
    expect(first.status).toBe(200);
    const firstBody = await first.json<{ id: string; inserted: boolean }>();
    expect(firstBody).toEqual({
      id: "e4cfa39a3d37be31c59609e807970799caa68a19bfaa15135f165085e01d41a65ba1e1b146aeb6bd0092b49eac214c103ccfa3a365954bbbe52f74a2b3620c94",
      inserted: true,
      references: [],
    });

    const second = await put("repo", "file", new TextEncoder().encode("hello"));
    expect(await second.json()).toMatchObject({ id: firstBody.id, inserted: false });
    expect(await env.REPOSITORIES.getByName("repo").countObjects()).toBe(1);
  });

  it("extracts a tree's file reference in the validation pass", async () => {
    const fileId = new Uint8Array(64).fill(7);
    const response = await put(
      "tree-repo",
      "tree",
      treeWithFiles([
        ["data", fileId],
        ["same", fileId],
      ]),
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({
      inserted: true,
      references: [{ kind: "file", id: "07".repeat(64) }],
    });
  });

  it("rejects malformed protobuf without poisoning the Wasm instance", async () => {
    const malformed = await put("malformed", "commit", new Uint8Array([0xff, 0xff]));
    expect(malformed.status).toBe(400);
    expect(await malformed.json()).toMatchObject({ error: expect.stringContaining("decode commit") });

    const emptyTree = await put("malformed", "tree", new Uint8Array());
    expect(emptyTree.status).toBe(200);
    expect(await emptyTree.json()).toMatchObject({
      id: "482ae5a29fbe856c7272f2071b8b0f0359ee2d89ff392b8a900643fbd0836eccd067b8bf41909e206c90d45d6e7d8b6686b93ecaee5fe1a9060d87b672101310",
    });
  });

  it("persists SQLite objects across Durable Object eviction", async () => {
    const stub = env.REPOSITORIES.getByName("eviction");
    await stub.validateAndStore("file", new TextEncoder().encode("durable"));
    expect(await stub.countObjects()).toBe(1);

    await evictDurableObject(stub);

    expect(await env.REPOSITORIES.getByName("eviction").countObjects()).toBe(1);
  });

  it("stores only the bytes in an RPC Uint8Array view", async () => {
    const stub = env.REPOSITORIES.getByName("subarray");
    const first = await stub.validateAndStore("file", new Uint8Array([0, 1, 2]).subarray(1, 2));
    const second = await stub.validateAndStore("file", new Uint8Array([1]));
    expect(first).toMatchObject({ ok: true, inserted: true });
    expect(second).toMatchObject({ ok: true, inserted: false });
  });

  it("enforces authorization, repository names, kinds and the body bound", async () => {
    const unauthorized = await exports.default.fetch(
      new Request("https://example.com/repositories/repo/objects/file", {
        method: "PUT",
        body: "hello",
      }),
    );
    expect(unauthorized.status).toBe(401);
    expect((await put("UPPER", "file", new Uint8Array())).status).toBe(400);
    expect((await put("repo", "unknown", new Uint8Array())).status).toBe(400);
    expect((await put("repo", "constructor", new Uint8Array())).status).toBe(400);
    expect(
      await env.REPOSITORIES.getByName("repo").validateAndStore("constructor", new Uint8Array()),
    ).toMatchObject({ ok: false, error: "unknown object kind: constructor" });
    expect((await put("large", "file", new Uint8Array(1024 * 1024 + 1))).status).toBe(400);
  });
});

async function put(repository: string, kind: string, bytes: Uint8Array): Promise<Response> {
  return exports.default.fetch(
    new Request(`https://example.com/repositories/${repository}/objects/${kind}`, {
      method: "PUT",
      headers: authorization,
      body: new Uint8Array(bytes).buffer,
    }),
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
  return concat(new Uint8Array([number << 3 | 2, bytes.byteLength]), bytes);
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
