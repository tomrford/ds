import type { AuthenticatedPrincipal, ControlPlane } from "./control_plane";
import {
  MAX_GIT_CHUNK_BYTES,
  MAX_GIT_MANIFEST_BYTES,
  readBoundedGitBody,
} from "./pack_git_protocol";
import type { RepositoryGit } from "./repository_git";
import { cursorStringSchema, lowerHexStringSchema } from "./validation";

const gitPackIdSchema = lowerHexStringSchema(64, "pack ID");

export async function routeGitRepository(
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
  const repositoryId = chunkMatch?.[1] ?? packMatch?.[1];
  const packId = chunkMatch?.[2] ?? packMatch?.[2];
  if (repositoryId === undefined || packId === undefined) return undefined;

  const authorization = await control.authorizeRepository(
    principal,
    repositoryId,
    request.headers.get("x-devspace-incarnation"),
  );
  if (!authorization.ok) return gitRpcResponse(authorization);
  if (!gitPackIdSchema.safeParse(packId).success) {
    return gitErrorResponse(400, "invalid pack ID");
  }

  const authority = authorization.authority;
  const stub: DurableObjectStub<RepositoryGit> = env.REPOSITORIES_GIT.getByName(repositoryId);
  const initialized = await stub.initializeRepository(authority);
  if (!initialized.ok) return gitRpcResponse(initialized);

  try {
    if (packMatch?.[3] === "manifest" && request.method === "PUT") {
      let bytes: Uint8Array;
      try {
        bytes = await readBoundedGitBody(request, MAX_GIT_MANIFEST_BYTES, "Git manifest");
      } catch (error) {
        return gitErrorResponse(
          400,
          error instanceof Error ? error.message : "invalid Git manifest body",
        );
      }
      return gitRpcResponse(await stub.putPackManifest(authority, packId, bytes));
    }
    if (chunkMatch !== null && request.method === "PUT") {
      const decodedPosition = cursorStringSchema.safeParse(chunkMatch[3]);
      if (!decodedPosition.success) return gitErrorResponse(400, "invalid chunk position");
      let bytes: Uint8Array;
      try {
        bytes = await readBoundedGitBody(request, MAX_GIT_CHUNK_BYTES, "Git chunk");
      } catch (error) {
        return gitErrorResponse(
          400,
          error instanceof Error ? error.message : "invalid Git chunk body",
        );
      }
      return gitRpcResponse(
        await stub.putPackChunk(authority, packId, decodedPosition.data, bytes),
      );
    }
    if (packMatch?.[3] === "install" && request.method === "POST") {
      return gitRpcResponse(await stub.installPack(authority, packId));
    }
    return gitErrorResponse(404, "not found");
  } catch (error) {
    console.error(
      JSON.stringify({
        message: "Git repository Durable Object failed",
        repositoryId,
        error: error instanceof Error ? error.name : "UnknownError",
      }),
    );
    return gitErrorResponse(500, "Git repository storage failed");
  }
}

function gitErrorResponse(status: number, message: string, code?: string): Response {
  const body = code === undefined ? { error: message } : { error: message, code };
  return Response.json(body, { status });
}

function gitRpcResponse(result: {
  ok: boolean;
  error?: string;
  code?: string;
  status?: number;
}): Response {
  if (!result.ok) {
    return gitErrorResponse(result.status ?? 400, result.error ?? "request failed", result.code);
  }
  const { ok: _, status: __, ...body } = result;
  return Response.json(body);
}
