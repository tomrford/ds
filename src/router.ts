import { MAX_HEAD_REQUEST_BYTES } from "./head_protocol";
import {
  MAX_CHUNK_BYTES,
  MAX_MANIFEST_BYTES,
  readBoundedBody,
} from "./pack_protocol";
import { MAX_PROJECTION_REQUEST_BYTES } from "./projection_protocol";
import { MAX_OBJECT_INVENTORY_REQUEST_BYTES } from "./object_protocol";
import { Env } from "./repository";

const REPOSITORY_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const PACK_ID_PATTERN = /^[0-9a-f]{128}$/;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const authorized =
      env.DEVSPACE_TOKEN &&
      (await tokenMatches(
        request.headers.get("authorization") ?? "",
        `Bearer ${env.DEVSPACE_TOKEN}`,
      ));
    if (!authorized) return errorResponse(401, "unauthorized");

    const url = new URL(request.url);
    const chunkMatch = /^\/repositories\/([^/]+)\/packs\/([^/]+)\/chunks\/([^/]+)$/.exec(
      url.pathname,
    );
    const packMatch = /^\/repositories\/([^/]+)\/packs\/([^/]+)\/(manifest|install)$/.exec(
      url.pathname,
    );
    const headMatch = /^\/repositories\/([^/]+)\/heads$/.exec(url.pathname);
    const projectionMatch = /^\/repositories\/([^/]+)\/projection$/.exec(url.pathname);
    const hiddenPolicyMatch = /^\/repositories\/([^/]+)\/hidden-policy$/.exec(url.pathname);
    const projectionPushMatch = /^\/repositories\/([^/]+)\/git\/pushes$/.exec(url.pathname);
    const projectionPushActionMatch =
      /^\/repositories\/([^/]+)\/git\/pushes\/([^/]+)\/(claim|confirm|recover|replay)$/.exec(
        url.pathname,
      );
    const packCatalogMatch = /^\/repositories\/([^/]+)\/packs$/.exec(url.pathname);
    const objectInventoryMatch = /^\/repositories\/([^/]+)\/objects\/inventory$/.exec(
      url.pathname,
    );
    const initializeMatch = /^\/repositories\/([^/]+)\/initialize$/.exec(url.pathname);
    const repository =
      chunkMatch?.[1] ??
      packMatch?.[1] ??
      headMatch?.[1] ??
      projectionMatch?.[1] ??
      hiddenPolicyMatch?.[1] ??
      projectionPushMatch?.[1] ??
      projectionPushActionMatch?.[1] ??
      packCatalogMatch?.[1] ??
      objectInventoryMatch?.[1] ??
      initializeMatch?.[1];
    const packId = chunkMatch?.[2] ?? packMatch?.[2];
    if (repository === undefined) return errorResponse(404, "not found");
    if (!REPOSITORY_PATTERN.test(repository)) return errorResponse(400, "invalid repository name");
    if (packId !== undefined && !PACK_ID_PATTERN.test(packId)) {
      return errorResponse(400, "invalid pack ID");
    }
    const stub = env.REPOSITORIES.getByName(repository);

    try {
      if (packMatch?.[3] === "manifest" && request.method === "PUT") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        let bytes: Uint8Array;
        try {
          bytes = await readBoundedBody(request, MAX_MANIFEST_BYTES, "manifest");
        } catch (error) {
          return errorResponse(400, error instanceof Error ? error.message : "invalid manifest body");
        }
        return rpcResponse(await stub.putPackManifest(packId, bytes));
      }
      if (objectInventoryMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_OBJECT_INVENTORY_REQUEST_BYTES,
          "object inventory request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.inventoryObjects(body));
      }
      if (packCatalogMatch !== null && request.method === "GET") {
        const after = url.searchParams.get("after") ?? "0";
        if (!/^(0|[1-9][0-9]*)$/.test(after) || !Number.isSafeInteger(Number(after))) {
          return errorResponse(400, "invalid pack cursor");
        }
        const throughValue = url.searchParams.get("through");
        if (
          throughValue !== null &&
          (!/^(0|[1-9][0-9]*)$/.test(throughValue) || !Number.isSafeInteger(Number(throughValue)))
        ) {
          return errorResponse(400, "invalid pack high-water");
        }
        return rpcResponse(
          await stub.listInstalledPacks(
            url.searchParams.get("incarnation"),
            Number(after),
            throughValue === null ? undefined : Number(throughValue),
          ),
        );
      }
      if (packMatch?.[3] === "manifest" && request.method === "GET") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        return binaryRpcResponse(
          await stub.getInstalledPackManifest(packId, url.searchParams.get("incarnation")),
        );
      }
      if (chunkMatch !== null && request.method === "PUT") {
        if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
        if (!/^(0|[1-9][0-9]*)$/.test(chunkMatch[3])) {
          return errorResponse(400, "invalid chunk position");
        }
        const position = Number(chunkMatch[3]);
        if (!Number.isSafeInteger(position)) return errorResponse(400, "invalid chunk position");
        let bytes: Uint8Array;
        try {
          bytes = await readBoundedBody(request, MAX_CHUNK_BYTES, "chunk");
        } catch (error) {
          return errorResponse(400, error instanceof Error ? error.message : "invalid chunk body");
        }
        return rpcResponse(await stub.putPackChunk(packId, position, bytes));
      }
      if (chunkMatch !== null && request.method === "GET") {
        if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
        if (!/^(0|[1-9][0-9]*)$/.test(chunkMatch[3])) {
          return errorResponse(400, "invalid chunk position");
        }
        const position = Number(chunkMatch[3]);
        if (!Number.isSafeInteger(position)) return errorResponse(400, "invalid chunk position");
        return binaryRpcResponse(
          await stub.getInstalledPackChunk(
            packId,
            position,
            url.searchParams.get("incarnation"),
          ),
        );
      }
      if (packMatch?.[3] === "install" && request.method === "POST") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        return rpcResponse(await stub.installPack(packId));
      }
      if (initializeMatch !== null && request.method === "POST") {
        return rpcResponse(
          await stub.initializeRepository(url.searchParams.get("incarnation")),
        );
      }
      if (headMatch !== null && request.method === "GET") {
        return rpcResponse(await stub.getHeads(url.searchParams.get("incarnation")));
      }
      if (headMatch !== null && request.method === "POST") {
        const decoded = await readJsonBody(request, MAX_HEAD_REQUEST_BYTES, "head request");
        if (decoded instanceof Response) return decoded;
        const body = decoded;
        return rpcResponse(await stub.transactHeads(body));
      }
      if (projectionMatch !== null && request.method === "GET") {
        const after = url.searchParams.get("after") ?? "0";
        const throughValue = url.searchParams.get("through");
        if (
          !/^(0|[1-9][0-9]*)$/.test(after) ||
          !Number.isSafeInteger(Number(after)) ||
          (throughValue !== null &&
            (!/^(0|[1-9][0-9]*)$/.test(throughValue) ||
              !Number.isSafeInteger(Number(throughValue))))
        ) {
          return errorResponse(400, "invalid projection cursor");
        }
        return rpcResponse(
          await stub.getProjection(
            url.searchParams.get("incarnation"),
            Number(after),
            throughValue === null ? undefined : Number(throughValue),
          ),
        );
      }
      if (hiddenPolicyMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "hidden policy request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.mutateHiddenPolicy(body));
      }
      if (projectionPushMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "projection push request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.beginProjectionPush(body));
      }
      if (
        projectionPushActionMatch?.[3] === "replay" &&
        request.method === "GET"
      ) {
        return rpcResponse(
          await stub.getProjectionPushReplay(
            projectionPushActionMatch[2],
            url.searchParams.get("incarnation"),
          ),
        );
      }
      if (projectionPushActionMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "projection push request",
        );
        if (body instanceof Response) return body;
        const batchId = projectionPushActionMatch[2];
        switch (projectionPushActionMatch[3]) {
          case "claim":
            return rpcResponse(await stub.claimProjectionPush(batchId, body));
          case "confirm":
            return rpcResponse(await stub.confirmProjectionPush(batchId, body));
          case "recover":
            return rpcResponse(await stub.recoverProjectionPush(batchId, body));
        }
      }
      return errorResponse(404, "not found");
    } catch (error) {
      console.error("repository Durable Object failed", error);
      return errorResponse(500, "repository storage failed");
    }
  },
} satisfies ExportedHandler<Env>;

function errorResponse(status: number, message: string): Response {
  return Response.json({ error: message }, { status });
}

async function tokenMatches(provided: string, expected: string): Promise<boolean> {
  const encoder = new TextEncoder();
  const [providedHash, expectedHash] = await Promise.all([
    crypto.subtle.digest("SHA-256", encoder.encode(provided)),
    crypto.subtle.digest("SHA-256", encoder.encode(expected)),
  ]);
  return crypto.subtle.timingSafeEqual(providedHash, expectedHash);
}

function rpcResponse(result: { ok: boolean; error?: string; status?: number }): Response {
  if (!result.ok) {
    return errorResponse(result.status ?? 400, result.error ?? "repository request failed");
  }
  const { ok: _, status: __, ...body } = result;
  return Response.json(body);
}

async function readJsonBody(
  request: Request,
  limit: number,
  label: string,
): Promise<unknown | Response> {
  let bytes: Uint8Array;
  try {
    bytes = await readBoundedBody(request, limit, label);
  } catch (error) {
    return errorResponse(400, error instanceof Error ? error.message : `invalid ${label}`);
  }
  try {
    return JSON.parse(
      new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes),
    );
  } catch {
    return errorResponse(400, `${label} must be valid JSON`);
  }
}

function binaryRpcResponse(result: {
  ok: boolean;
  error?: string;
  status?: number;
  bytes?: ArrayBuffer;
}): Response {
  if (!result.ok) {
    return errorResponse(result.status ?? 400, result.error ?? "repository request failed");
  }
  if (result.bytes === undefined) throw new Error("binary repository response is missing bytes");
  return new Response(result.bytes, { headers: { "content-type": "application/octet-stream" } });
}
