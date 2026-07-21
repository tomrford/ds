import { authenticateDevelopmentRequest, type DevelopmentSecretEnv } from "./auth";
import { MAX_HEAD_REQUEST_BYTES } from "./head_protocol";
import { MAX_OBJECT_INVENTORY_REQUEST_BYTES } from "./object_protocol";
import { MAX_CHUNK_BYTES, MAX_MANIFEST_BYTES, readBoundedBody } from "./pack_protocol";
import { MAX_PROJECTION_REQUEST_BYTES } from "./projection_protocol";
import { MAX_REMOTE_REQUEST_BYTES } from "./remote_protocol";
import type { Repository } from "./repository";
import {
  cursorStringSchema,
  objectIdStringSchema,
} from "./validation";
import { z } from "zod";

const DIRECTORY_REQUEST_BYTES = 4 * 1024;
const repositoryNameSchema = z.string().regex(/^[a-z0-9][a-z0-9._-]{0,127}$/);
const CONTROL_PLANE_NAME = "directory";
type WorkerEnv = Env & DevelopmentSecretEnv;

export default {
  async fetch(request: Request, env: WorkerEnv): Promise<Response> {
    try {
      return await route(request, env);
    } catch (error) {
      console.error(
        JSON.stringify({
          message: "Worker request failed",
          path: new URL(request.url).pathname,
          error: error instanceof Error ? error.name : "UnknownError",
        }),
      );
      return errorResponse(500, "request failed");
    }
  },
} satisfies ExportedHandler<WorkerEnv>;

async function route(request: Request, env: WorkerEnv): Promise<Response> {
    const authentication = await authenticateDevelopmentRequest(request, env);
    if (!authentication.ok) {
      return errorResponse(authentication.status, authentication.error, authentication.code);
    }
    const principal = authentication.principal;
    const control = env.CONTROL_PLANE.getByName(CONTROL_PLANE_NAME);
    const url = new URL(request.url);

    const chunkMatch = /^\/repositories\/([^/]+)\/packs\/([^/]+)\/chunks\/([^/]+)$/.exec(
      url.pathname,
    );
    const packMatch = /^\/repositories\/([^/]+)\/packs\/([^/]+)\/(manifest|install)$/.exec(
      url.pathname,
    );
    const headMatch = /^\/repositories\/([^/]+)\/heads$/.exec(url.pathname);
    const projectionMatch = /^\/repositories\/([^/]+)\/projection$/.exec(url.pathname);
    const remotesMatch = /^\/repositories\/([^/]+)\/remotes$/.exec(url.pathname);
    const remoteMatch = /^\/repositories\/([^/]+)\/remotes\/([^/]+)$/.exec(url.pathname);
    const projectionPushMatch = /^\/repositories\/([^/]+)\/git\/pushes$/.exec(url.pathname);
    const projectionFetchMatch = /^\/repositories\/([^/]+)\/git\/fetches$/.exec(url.pathname);
    const projectionPushActionMatch =
      /^\/repositories\/([^/]+)\/git\/pushes\/([^/]+)\/(claim|recover|replay)$/.exec(
        url.pathname,
      );
    const packCatalogMatch = /^\/repositories\/([^/]+)\/packs$/.exec(url.pathname);
    const objectInventoryMatch = /^\/repositories\/([^/]+)\/objects\/inventory$/.exec(
      url.pathname,
    );
    const repositoryId =
      chunkMatch?.[1] ??
      packMatch?.[1] ??
      headMatch?.[1] ??
      projectionMatch?.[1] ??
      remotesMatch?.[1] ??
      remoteMatch?.[1] ??
      projectionPushMatch?.[1] ??
      projectionFetchMatch?.[1] ??
      projectionPushActionMatch?.[1] ??
      packCatalogMatch?.[1] ??
      objectInventoryMatch?.[1];
    const packId = chunkMatch?.[2] ?? packMatch?.[2];
    if (repositoryId === undefined) {
      const directoryMatch = /^\/repositories\/([^/]+)$/.exec(url.pathname);
      if (url.pathname === "/repositories" && request.method === "POST") {
        const body = await readJsonBody(
          request,
          DIRECTORY_REQUEST_BYTES,
          "repository creation request",
          "invalid-repository-creation-request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await control.createRepository(principal, body));
      }
      if (url.pathname === "/repositories" && request.method === "GET") {
        return rpcResponse(await control.listRepositories(principal));
      }
      if (directoryMatch !== null) {
        const name = directoryMatch[1];
        if (!repositoryNameSchema.safeParse(name).success) {
          return errorResponse(400, "invalid repository name", "invalid-repository-name");
        }
        if (request.method === "GET") {
          return rpcResponse(await control.resolveRepository(principal, name));
        }
        if (request.method === "PATCH") {
          const body = await readJsonBody(
            request,
            DIRECTORY_REQUEST_BYTES,
            "repository rename request",
            "invalid-repository-rename-request",
          );
          if (body instanceof Response) return body;
          return rpcResponse(await control.renameRepository(principal, name, body));
        }
        if (request.method === "DELETE") {
          const body = await readJsonBody(
            request,
            DIRECTORY_REQUEST_BYTES,
            "repository deletion request",
            "invalid-control-plane-request",
          );
          if (body instanceof Response) return body;
          return rpcResponse(await control.deleteRepository(principal, name, body));
        }
      }
      return errorResponse(404, "not found");
    }
    const incarnation = request.headers.get("x-devspace-incarnation");
    const authorization = await control.authorizeRepository(
      principal,
      repositoryId,
      incarnation,
    );
    if (!authorization.ok) return rpcResponse(authorization);
    if (packId !== undefined && !objectIdStringSchema.safeParse(packId).success) {
      return errorResponse(400, "invalid pack ID");
    }
    const authority = authorization.authority;
    let stub: DurableObjectStub<Repository>;
    try {
      stub = env.REPOSITORIES.get(env.REPOSITORIES.idFromString(repositoryId));
    } catch {
      return errorResponse(404, "repository not found");
    }

    try {
      if (packMatch?.[3] === "manifest" && request.method === "PUT") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        let bytes: Uint8Array;
        try {
          bytes = await readBoundedBody(request, MAX_MANIFEST_BYTES, "manifest");
        } catch (error) {
          return errorResponse(400, error instanceof Error ? error.message : "invalid manifest body");
        }
        return rpcResponse(await stub.putPackManifest(authority, packId, bytes));
      }
      if (objectInventoryMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_OBJECT_INVENTORY_REQUEST_BYTES,
          "object inventory request",
          "invalid-object-inventory-request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.inventoryObjects(authority, body));
      }
      if (packCatalogMatch !== null && request.method === "GET") {
        const after = cursorStringSchema.safeParse(url.searchParams.get("after") ?? "0");
        if (!after.success) {
          return errorResponse(400, "invalid pack cursor");
        }
        const throughValue = url.searchParams.get("through");
        const through = throughValue === null ? undefined : cursorStringSchema.safeParse(throughValue);
        if (through !== undefined && !through.success) {
          return errorResponse(400, "invalid pack high-water");
        }
        return rpcResponse(
          await stub.listInstalledPacks(
            authority,
            url.searchParams.get("incarnation"),
            after.data,
            through?.data,
          ),
        );
      }
      if (packMatch?.[3] === "manifest" && request.method === "GET") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        return binaryRpcResponse(
          await stub.getInstalledPackManifest(
            authority,
            packId,
            url.searchParams.get("incarnation"),
          ),
        );
      }
      if (chunkMatch !== null) {
        if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
        const decodedPosition = cursorStringSchema.safeParse(chunkMatch[3]);
        if (!decodedPosition.success) {
          return errorResponse(400, "invalid chunk position");
        }
        const position = decodedPosition.data;
        if (request.method === "PUT") {
          let bytes: Uint8Array;
          try {
            bytes = await readBoundedBody(request, MAX_CHUNK_BYTES, "chunk");
          } catch (error) {
            return errorResponse(400, error instanceof Error ? error.message : "invalid chunk body");
          }
          return rpcResponse(await stub.putPackChunk(authority, packId, position, bytes));
        }
        if (request.method === "GET") {
          return binaryRpcResponse(
            await stub.getInstalledPackChunk(
              authority,
              packId,
              position,
              url.searchParams.get("incarnation"),
            ),
          );
        }
      }
      if (packMatch?.[3] === "install" && request.method === "POST") {
        if (packId === undefined) throw new Error("pack route did not capture an ID");
        return rpcResponse(await stub.installPack(authority, packId));
      }
      if (headMatch !== null && request.method === "GET") {
        return rpcResponse(await stub.getHeads(authority, url.searchParams.get("incarnation")));
      }
      if (headMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_HEAD_REQUEST_BYTES,
          "head request",
          "invalid-head-request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.transactHeads(authority, body));
      }
      if (projectionMatch !== null && request.method === "GET") {
        const after = cursorStringSchema.safeParse(url.searchParams.get("after") ?? "0");
        const throughValue = url.searchParams.get("through");
        const through = throughValue === null ? undefined : cursorStringSchema.safeParse(throughValue);
        if (!after.success || (through !== undefined && !through.success)) {
          return errorResponse(400, "invalid projection cursor");
        }
        return rpcResponse(
          await stub.getProjection(
            authority,
            url.searchParams.get("incarnation"),
            after.data,
            through?.data,
          ),
        );
      }
      if (remotesMatch !== null && request.method === "GET") {
        return rpcResponse(
          await stub.listRemotes(authority, url.searchParams.get("incarnation")),
        );
      }
      if (remoteMatch !== null && request.method === "PUT") {
        let name: string;
        try {
          name = decodeURIComponent(remoteMatch[2]);
        } catch {
          return errorResponse(400, "remote name has invalid URL encoding", "invalid-remote-name");
        }
        const body = await readJsonBody(
          request,
          MAX_REMOTE_REQUEST_BYTES,
          "remote request",
          "invalid-remote-request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.setRemote(authority, name, body));
      }
      if (projectionPushMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "projection push request",
          "invalid-projection-request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.beginProjectionPush(authority, body));
      }
      if (projectionFetchMatch !== null && request.method === "POST") {
        const body = await readJsonBody(
          request,
          MAX_PROJECTION_REQUEST_BYTES,
          "projection fetch request",
          "invalid-fetch-request",
        );
        if (body instanceof Response) return body;
        return rpcResponse(await stub.recordProjectionFetch(authority, body));
      }
      if (projectionPushActionMatch?.[3] === "replay" && request.method === "GET") {
        return rpcResponse(
          await stub.getProjectionPushReplay(
            authority,
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
          "invalid-projection-request",
        );
        if (body instanceof Response) return body;
        const batchId = projectionPushActionMatch[2];
        switch (projectionPushActionMatch[3]) {
          case "claim":
            return rpcResponse(await stub.claimProjectionPush(authority, batchId, body));
          case "recover":
            return rpcResponse(await stub.recoverProjectionPush(authority, batchId, body));
        }
      }
      return errorResponse(404, "not found");
    } catch (error) {
      console.error("repository Durable Object failed", error);
      return errorResponse(500, "repository storage failed");
    }
}

function errorResponse(status: number, message: string, code?: string): Response {
  const body = code === undefined ? { error: message } : { error: message, code };
  return Response.json(body, { status });
}

function rpcResponse(result: { ok: boolean; error?: string; code?: string; status?: number }): Response {
  if (!result.ok) {
    return errorResponse(result.status ?? 400, result.error ?? "request failed", result.code);
  }
  const { ok: _, status: __, ...body } = result;
  return Response.json(body);
}

async function readJsonBody(
  request: Request,
  limit: number,
  label: string,
  code: string,
): Promise<unknown | Response> {
  let bytes: Uint8Array;
  try {
    bytes = await readBoundedBody(request, limit, label);
  } catch (error) {
    return errorResponse(400, error instanceof Error ? error.message : `invalid ${label}`, code);
  }
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
  } catch {
    return errorResponse(400, `${label} must be valid JSON`, code);
  }
}

function binaryRpcResponse(result: { ok: boolean; error?: string; status?: number; bytes?: ArrayBuffer }): Response {
  if (!result.ok) return errorResponse(result.status ?? 400, result.error ?? "repository request failed");
  if (result.bytes === undefined) throw new Error("binary repository response is missing bytes");
  return new Response(result.bytes, { headers: { "content-type": "application/octet-stream" } });
}
