import type { AuthenticatedPrincipal } from "./control_plane";

const MACHINE_ID_PATTERN = /^[0-9a-f]{32}$/;
const USER_ID_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;

/**
 * Traditional Worker secrets are configured with `wrangler secret put`, so
 * they are intentionally absent from wrangler.jsonc and its generated Env.
 */
export interface DevelopmentSecretEnv {
  DEVSPACE_SHARED_SECRET: string;
}

type AuthenticationResult =
  | { ok: true; principal: AuthenticatedPrincipal }
  | { ok: false; status: number; error: string };

/**
 * Development-only HTTP trust boundary. The shared secret selects one fixed
 * server-side user; callers supply only their non-authoritative machine ID.
 */
export async function authenticateDevelopmentRequest(
  request: Request,
  env: Env & DevelopmentSecretEnv,
): Promise<AuthenticationResult> {
  const authorization = request.headers.get("authorization");
  const provided = authorization === null ? undefined : /^Bearer ([^ ]+)$/.exec(authorization)?.[1];
  const expected = env.DEVSPACE_SHARED_SECRET;
  if (
    provided === undefined ||
    typeof expected !== "string" ||
    expected.length === 0 ||
    !(await secretsMatch(provided, expected))
  ) {
    return { ok: false, status: 401, error: "unauthorized" };
  }

  const machineId = request.headers.get("x-devspace-machine-id");
  if (machineId === null || !MACHINE_ID_PATTERN.test(machineId)) {
    return { ok: false, status: 400, error: "invalid machine ID" };
  }
  const userId = env.DEVSPACE_DEVELOPMENT_USER_ID;
  if (!USER_ID_PATTERN.test(userId)) {
    return { ok: false, status: 500, error: "development authentication is not configured" };
  }
  return { ok: true, principal: { userId, machineId } };
}

async function secretsMatch(provided: string, expected: string): Promise<boolean> {
  const encoder = new TextEncoder();
  const [providedHash, expectedHash] = await Promise.all([
    crypto.subtle.digest("SHA-256", encoder.encode(provided)),
    crypto.subtle.digest("SHA-256", encoder.encode(expected)),
  ]);
  return crypto.subtle.timingSafeEqual(providedHash, expectedHash);
}
