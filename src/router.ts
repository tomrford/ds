import { z } from "zod";
import { authenticateDevelopmentRequest, type DevelopmentSecretEnv } from "./auth";
import type { AuthenticatedPrincipal, ControlPlane } from "./control_plane";
import {
  MAX_GIT_CHUNK_BYTES,
  MAX_GIT_MANIFEST_BYTES,
  readBoundedGitBody,
} from "./pack_protocol";
import { MAX_GIT_PROJECTION_REQUEST_BYTES } from "./projection_protocol";
import { MAX_REMOTE_REQUEST_BYTES } from "./remote_protocol";
import {
  MAX_OP_INVENTORY_REQUEST_BYTES,
  MAX_OP_OBJECT_BYTES,
} from "./op_store";
import { MAX_HEAD_REQUEST_BYTES } from "./op_protocol";
import type { RepositoryGit } from "./repository";
import { cursorStringSchema, lowerHexStringSchema } from "./validation";

const gitPackIdSchema = lowerHexStringSchema(64, "pack ID");
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
  const repositoryResponse = await routeRepository(request, env, principal, control, url);
  if (repositoryResponse !== undefined) return repositoryResponse;

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

async function routeRepository(
  request: Request,
  env: Env,
  principal: AuthenticatedPrincipal,
  control: DurableObjectStub<ControlPlane>,
  url: URL,
): Promise<Response | undefined> {
  const chunkMatch = /^\/repositories\/([^/]+)\/git\/packs\/([^/]+)\/chunks\/([^/]+)$/.exec(
    url.pathname,
  );
  const packMatch = /^\/repositories\/([^/]+)\/git\/packs\/([^/]+)\/(manifest|install)$/.exec(
    url.pathname,
  );
  const packCatalogMatch = /^\/repositories\/([^/]+)\/git\/packs$/.exec(url.pathname);
  const projectionMatch = /^\/repositories\/([^/]+)\/git\/projection$/.exec(url.pathname);
  const remotesMatch = /^\/repositories\/([^/]+)\/git\/remotes$/.exec(url.pathname);
  const remoteMatch = /^\/repositories\/([^/]+)\/git\/remotes\/([^/]+)$/.exec(url.pathname);
  const projectionPushMatch = /^\/repositories\/([^/]+)\/git\/projection\/pushes$/.exec(
    url.pathname,
  );
  const projectionFetchMatch = /^\/repositories\/([^/]+)\/git\/projection\/fetches$/.exec(
    url.pathname,
  );
  const projectionPushActionMatch =
    /^\/repositories\/([^/]+)\/git\/projection\/pushes\/([^/]+)\/(claim|recover|replay)$/.exec(
      url.pathname,
    );
  const opObjectMatch =
    /^\/repositories\/([^/]+)\/git\/ops\/(views|operations)\/([^/]+)$/.exec(url.pathname);
  const opInventoryMatch =
    /^\/repositories\/([^/]+)\/git\/ops\/inventory$/.exec(url.pathname);
  const opHeadsMatch =
    /^\/repositories\/([^/]+)\/git\/ops\/heads$/.exec(url.pathname);
  const opHeadTransactionsMatch =
    /^\/repositories\/([^/]+)\/git\/ops\/heads\/transactions$/.exec(url.pathname);
  const repositoryId =
    chunkMatch?.[1] ??
    packMatch?.[1] ??
    packCatalogMatch?.[1] ??
    projectionMatch?.[1] ??
    remotesMatch?.[1] ??
    remoteMatch?.[1] ??
    projectionPushMatch?.[1] ??
    projectionFetchMatch?.[1] ??
    projectionPushActionMatch?.[1] ??
    opObjectMatch?.[1] ??
    opInventoryMatch?.[1] ??
    opHeadsMatch?.[1] ??
    opHeadTransactionsMatch?.[1];
  const packId = chunkMatch?.[2] ?? packMatch?.[2];
  if (repositoryId === undefined) return undefined;

  const authorization = await control.authorizeRepository(
    principal,
    repositoryId,
    request.headers.get("x-devspace-incarnation"),
  );
  if (!authorization.ok) return rpcResponse(authorization);
  if (packId !== undefined && !gitPackIdSchema.safeParse(packId).success) {
    return errorResponse(400, "invalid pack ID");
  }

  const authority = authorization.authority;
  const stub: DurableObjectStub<RepositoryGit> = env.REPOSITORIES.getByName(repositoryId);
  const initialized = await stub.initializeRepository(authority);
  if (!initialized.ok) return rpcResponse(initialized);

  try {
    if (opObjectMatch !== null && (request.method === "PUT" || request.method === "GET")) {
      const kind = opObjectMatch[2] === "views" ? "view" : "operation";
      const id = opObjectMatch[3];
      if (request.method === "GET") {
        return binaryRpcResponse(await stub.getOpObject(authority, kind, id));
      }
      let bytes: Uint8Array;
      try {
        bytes = await readBoundedGitBody(request, MAX_OP_OBJECT_BYTES, "operation-store object");
      } catch (error) {
        return errorResponse(
          400,
          error instanceof Error ? error.message : "invalid operation-store object body",
        );
      }
      return rpcResponse(await stub.putOpObject(authority, kind, id, bytes));
    }
    if (opInventoryMatch !== null && request.method === "POST") {
      const body = await readJsonBody(
        request,
        MAX_OP_INVENTORY_REQUEST_BYTES,
        "operation-store inventory request",
        "invalid-op-inventory",
      );
      if (body instanceof Response) return body;
      return rpcResponse(await stub.inventoryOpObjects(authority, body));
    }
    if (opHeadsMatch !== null && request.method === "GET") {
      return rpcResponse(await stub.getOpHeads(authority));
    }
    if (opHeadTransactionsMatch !== null && request.method === "POST") {
      const body = await readJsonBody(
        request,
        MAX_HEAD_REQUEST_BYTES,
        "operation head request",
        "invalid-head-request",
      );
      if (body instanceof Response) return body;
      return rpcResponse(await stub.transactOpHeads(authority, body));
    }
    if (packCatalogMatch !== null && request.method === "GET") {
      const after = cursorStringSchema.safeParse(url.searchParams.get("after") ?? "0");
      if (!after.success) return errorResponse(400, "invalid pack cursor");
      const throughValue = url.searchParams.get("through");
      const through = throughValue === null ? undefined : cursorStringSchema.safeParse(throughValue);
      if (through !== undefined && !through.success) {
        return errorResponse(400, "invalid pack high-water");
      }
      return rpcResponse(await stub.listInstalledPacks(authority, after.data, through?.data));
    }
    if (packMatch?.[3] === "manifest" && request.method === "PUT") {
      if (packId === undefined) throw new Error("pack route did not capture an ID");
      let bytes: Uint8Array;
      try {
        bytes = await readBoundedGitBody(request, MAX_GIT_MANIFEST_BYTES, "Git manifest");
      } catch (error) {
        return errorResponse(
          400,
          error instanceof Error ? error.message : "invalid Git manifest body",
        );
      }
      return rpcResponse(await stub.putPackManifest(authority, packId, bytes));
    }
    if (packMatch?.[3] === "manifest" && request.method === "GET") {
      if (packId === undefined) throw new Error("pack route did not capture an ID");
      return binaryRpcResponse(await stub.getInstalledPackManifest(authority, packId));
    }
    if (chunkMatch !== null && request.method === "PUT") {
      if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
      const decodedPosition = cursorStringSchema.safeParse(chunkMatch[3]);
      if (!decodedPosition.success) return errorResponse(400, "invalid chunk position");
      let bytes: Uint8Array;
      try {
        bytes = await readBoundedGitBody(request, MAX_GIT_CHUNK_BYTES, "Git chunk");
      } catch (error) {
        return errorResponse(
          400,
          error instanceof Error ? error.message : "invalid Git chunk body",
        );
      }
      return rpcResponse(
        await stub.putPackChunk(authority, packId, decodedPosition.data, bytes),
      );
    }
    if (chunkMatch !== null && request.method === "GET") {
      if (packId === undefined) throw new Error("chunk route did not capture a pack ID");
      const decodedPosition = cursorStringSchema.safeParse(chunkMatch[3]);
      if (!decodedPosition.success) return errorResponse(400, "invalid chunk position");
      return binaryRpcResponse(
        await stub.getInstalledPackChunk(authority, packId, decodedPosition.data),
      );
    }
    if (packMatch?.[3] === "install" && request.method === "POST") {
      if (packId === undefined) throw new Error("pack route did not capture an ID");
      return rpcResponse(await stub.installPack(authority, packId));
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
        MAX_GIT_PROJECTION_REQUEST_BYTES,
        "projection push request",
        "invalid-projection-request",
      );
      if (body instanceof Response) return body;
      return rpcResponse(await stub.beginProjectionPush(authority, body));
    }
    if (projectionFetchMatch !== null && request.method === "POST") {
      const body = await readJsonBody(
        request,
        MAX_GIT_PROJECTION_REQUEST_BYTES,
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
        MAX_GIT_PROJECTION_REQUEST_BYTES,
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
    console.error(
      JSON.stringify({
        message: "Git repository Durable Object failed",
        repositoryId,
        error: error instanceof Error ? error.name : "UnknownError",
      }),
    );
    return errorResponse(500, "Git repository storage failed");
  }
}

function binaryRpcResponse(result: {
  ok: boolean;
  bytes?: ArrayBuffer;
  error?: string;
  code?: string;
  status?: number;
}): Response {
  if (!result.ok || result.bytes === undefined) {
    return errorResponse(result.status ?? 400, result.error ?? "request failed", result.code);
  }
  return new Response(result.bytes, {
    headers: { "content-type": "application/octet-stream" },
  });
}

function errorResponse(status: number, message: string, code?: string): Response {
  const body = code === undefined ? { error: message } : { error: message, code };
  return Response.json(body, { status });
}

function rpcResponse(result: {
  ok: boolean;
  error?: string;
  code?: string;
  status?: number;
}): Response {
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
    bytes = await readBoundedGitBody(request, limit, label);
  } catch (error) {
    return errorResponse(400, error instanceof Error ? error.message : `invalid ${label}`, code);
  }
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes));
  } catch {
    return errorResponse(400, `${label} must be valid JSON`, code);
  }
}
