import { DurableObject } from "cloudflare:workers";
import { toHex } from "./kernel";

const ID_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const MACHINE_ID_PATTERN = /^[0-9a-f]{32}$/;
const REPOSITORY_NAME_PATTERN = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const REPOSITORY_ID_PATTERN = /^[0-9a-f]{64}$/;
const INCARNATION_PATTERN = /^[0-9a-f]{32}$/;
const IDEMPOTENCY_KEY_PATTERN = /^[0-9a-f]{32}$/;
const PROVISIONAL_RETENTION_MS = 24 * 60 * 60 * 1_000;
const RETIREMENT_RECOVERY_BATCH = 64;
const NAME_IN_USE = "name-in-use";
const CREATION_RETIRED = "creation-retired";
const CREATION_RETIRING = "creation-retiring";
const IDEMPOTENCY_KEY_REUSED = "idempotency-key-reused";
const REPOSITORY_NOT_FOUND = "repository-not-found";

type ControlPlaneErrorCode =
  | typeof NAME_IN_USE
  | typeof CREATION_RETIRED
  | typeof CREATION_RETIRING
  | typeof IDEMPOTENCY_KEY_REUSED
  | typeof REPOSITORY_NOT_FOUND;

export interface AuthenticatedPrincipal {
  userId: string;
  machineId: string;
}

export interface RepositoryAuthority extends AuthenticatedPrincipal {
  repositoryId: string;
  incarnation: string;
}

interface RepositoryRow extends Record<string, SqlStorageValue> {
  repository_id: string;
  user_id: string;
  name: string;
  incarnation: string;
  status: string;
}

interface CreationReceiptRow extends Record<string, SqlStorageValue> {
  name: string;
  repository_id: string;
  incarnation: string;
}

class ControlPlaneError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code?: ControlPlaneErrorCode,
  ) {
    super(message);
  }
}

export class ControlPlane extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.ctx.blockConcurrencyWhile(async () =>
      this.ctx.storage.transactionSync(() => initializeControlPlaneSchema(this.ctx.storage.sql)),
    );
  }

  authorizeRepository(
    identityValue: unknown,
    repositoryIdValue: unknown,
    incarnationValue: unknown,
  ) {
    let identity: AuthenticatedPrincipal;
    try {
      identity = decodeIdentity(identityValue);
    } catch (error) {
      return failure(error, 400);
    }
    if (typeof repositoryIdValue !== "string" || !REPOSITORY_ID_PATTERN.test(repositoryIdValue)) {
      return failure("repository not found", 404);
    }
    let incarnation: string;
    try {
      incarnation = requireIncarnation(incarnationValue);
    } catch (error) {
      return failure(error, 400);
    }
    const repository = this.repositoryById(identity.userId, repositoryIdValue);
    if (
      repository === undefined ||
      repository.status !== "active" ||
      repository.incarnation !== incarnation
    ) {
      return failure("repository not found", 404);
    }
    return {
      ok: true as const,
      authority: { ...identity, repositoryId: repositoryIdValue, incarnation },
    };
  }

  async createRepository(identityValue: unknown, value: unknown) {
    let identity: AuthenticatedPrincipal;
    let request: { name: string; idempotencyKey: string };
    try {
      identity = decodeIdentity(identityValue);
      request = decodeCreateRepository(value);
    } catch (error) {
      return failure(error, 400);
    }

    const retiring = this.ctx.storage.transactionSync(() => {
      const now = Date.now();
      this.ctx.storage.sql.exec(
        `UPDATE repositories SET status = 'retiring'
         WHERE user_id = ? AND status = 'provisional' AND created_at_ms < ?`,
        identity.userId,
        now - PROVISIONAL_RETENTION_MS,
      );
      return this.ctx.storage.sql
        .exec<RepositoryRow>(
          `SELECT repository_id, user_id, name, incarnation, status FROM repositories
           WHERE user_id = ? AND status = 'retiring'
           ORDER BY CASE WHEN name = ? THEN 0 ELSE 1 END, created_at_ms LIMIT ?`,
          identity.userId,
          request.name,
          RETIREMENT_RECOVERY_BATCH,
        )
        .toArray();
    });
    await this.recoverRetiringRepositories(identity.machineId, retiring);

    let creation: { authority: RepositoryAuthority; needsActivation: boolean };
    try {
      creation = this.ctx.storage.transactionSync(() => {
        const now = Date.now();
        const receipt = this.ctx.storage.sql
          .exec<CreationReceiptRow>(
            `SELECT name, repository_id, incarnation FROM repository_creation_receipts
             WHERE user_id = ? AND idempotency_key = ?`,
            identity.userId,
            request.idempotencyKey,
          )
          .toArray()[0];
        if (receipt !== undefined) {
          if (receipt.name !== request.name) {
            throw new ControlPlaneError(
              "idempotency key was already used for a different repository request",
              409,
              IDEMPOTENCY_KEY_REUSED,
            );
          }
          const repository = this.repositoryById(identity.userId, receipt.repository_id);
          if (repository === undefined || repository.status === "deleted") {
            throw new ControlPlaneError(
              "repository created by this request was retired",
              409,
              CREATION_RETIRED,
            );
          }
          if (repository.status === "retiring") {
            throw new ControlPlaneError(
              "repository created by this request is retiring",
              409,
              CREATION_RETIRING,
            );
          }
          return {
            authority: {
              ...identity,
              repositoryId: receipt.repository_id,
              incarnation: receipt.incarnation,
            },
            needsActivation: repository.status === "provisional",
          };
        }

        const occupied = this.ctx.storage.sql
          .exec<{ count: number }>(
            `SELECT count(*) AS count FROM repositories
             WHERE user_id = ? AND name = ? AND status != 'deleted'`,
            identity.userId,
            request.name,
          )
          .one().count;
        if (occupied !== 0) {
          throw new ControlPlaneError("repository name is already in use", 409, NAME_IN_USE);
        }

        const repositoryId = this.env.REPOSITORIES.newUniqueId().toString();
        const incarnation = randomHex(16);
        this.ctx.storage.sql.exec(
          "INSERT INTO repositories VALUES (?, ?, ?, ?, 'provisional', ?, NULL)",
          repositoryId,
          identity.userId,
          request.name,
          incarnation,
          now,
        );
        this.ctx.storage.sql.exec(
          "INSERT INTO repository_creation_receipts VALUES (?, ?, ?, ?, ?)",
          identity.userId,
          request.idempotencyKey,
          request.name,
          repositoryId,
          incarnation,
        );
        return {
          authority: { ...identity, repositoryId, incarnation },
          needsActivation: true,
        };
      });
    } catch (error) {
      return expectedFailure(error);
    }

    const authority = creation.authority;
    if (!creation.needsActivation) return creationSuccess(authority, request.name);

    const stub = this.env.REPOSITORIES.get(
      this.env.REPOSITORIES.idFromString(authority.repositoryId),
    );
    const initialized = await stub.initializeRepository(authority);
    if (!initialized.ok) return initialized;
    try {
      this.ctx.storage.transactionSync(() => {
        const changed = this.ctx.storage.sql
          .exec<{ repository_id: string }>(
            `UPDATE repositories SET status = 'active'
             WHERE user_id = ? AND repository_id = ? AND incarnation = ? AND status = 'provisional'
             RETURNING repository_id`,
            identity.userId,
            authority.repositoryId,
            authority.incarnation,
          )
          .toArray();
        if (changed.length === 1) return;
        const repository = this.repositoryById(identity.userId, authority.repositoryId);
        if (
          changed.length === 0 &&
          repository?.status === "active" &&
          repository.incarnation === authority.incarnation
        ) {
          return;
        }
        if (changed.length !== 0) {
          throw new Error("repository activation changed more than one row");
        }
        if (
          repository === undefined ||
          repository.status === "retiring" ||
          repository.status === "deleted"
        ) {
          throw new ControlPlaneError("repository creation was retired", 409);
        }
        throw new Error("repository activation did not change exactly one provisional row");
      });
      return creationSuccess(authority, request.name);
    } catch (error) {
      return expectedFailure(error);
    }
  }

  resolveRepository(identityValue: unknown, nameValue: unknown) {
    let identity: AuthenticatedPrincipal;
    let name: string;
    try {
      identity = decodeIdentity(identityValue);
      name = requireRepositoryName(nameValue);
    } catch (error) {
      return failure(error, 400);
    }
    const repository = this.ctx.storage.sql
      .exec<RepositoryRow>(
        `SELECT repository_id, user_id, name, incarnation, status FROM repositories
         WHERE user_id = ? AND name = ? AND status = 'active'`,
        identity.userId,
        name,
      )
      .toArray()[0];
    if (repository === undefined) {
      return failure(new ControlPlaneError("repository not found", 404, REPOSITORY_NOT_FOUND), 404);
    }
    return {
      ok: true as const,
      name: repository.name,
      repositoryId: repository.repository_id,
      incarnation: repository.incarnation,
    };
  }

  /** Test-only lifecycle seam. It has no Worker route. */
  beginTestRepositoryRetirement(identityValue: unknown, repositoryIdValue: unknown) {
    let identity: AuthenticatedPrincipal;
    let repositoryId: string;
    try {
      identity = decodeIdentity(identityValue);
      repositoryId = requireRepositoryId(repositoryIdValue);
    } catch (error) {
      return failure(error, 400);
    }
    try {
      return this.ctx.storage.transactionSync(() => {
        const changed = this.ctx.storage.sql
          .exec<{ repository_id: string }>(
            `UPDATE repositories SET status = 'retiring'
             WHERE user_id = ? AND repository_id = ? AND status = 'active'
             RETURNING repository_id`,
            identity.userId,
            repositoryId,
          )
          .toArray();
        if (changed.length !== 1) {
          throw new ControlPlaneError("active repository not found", 404);
        }
        return { ok: true as const, retiring: true };
      });
    } catch (error) {
      return expectedFailure(error);
    }
  }

  async deleteRepository(identityValue: unknown, value: unknown) {
    let identity: AuthenticatedPrincipal;
    let request: { name: string; repositoryId: string; incarnation: string };
    try {
      identity = decodeIdentity(identityValue);
      request = decodeDeleteRepository(value);
    } catch (error) {
      return failure(error, 400);
    }
    let status: string;
    try {
      status = this.ctx.storage.transactionSync(() => {
        const repository = this.repositoryById(identity.userId, request.repositoryId);
        if (
          repository === undefined ||
          repository.name !== request.name ||
          repository.incarnation !== request.incarnation
        ) {
          throw new ControlPlaneError("repository not found", 404);
        }
        if (repository.status === "deleted") return repository.status;
        if (repository.status === "provisional") {
          throw new ControlPlaneError("repository creation is still provisional", 409);
        }
        if (repository.status === "active") {
          const changed = this.ctx.storage.sql
            .exec<{ repository_id: string }>(
              `UPDATE repositories SET status = 'retiring'
               WHERE repository_id = ? AND status = 'active'
               RETURNING repository_id`,
              request.repositoryId,
            )
            .toArray();
          if (changed.length !== 1) {
            throw new Error("repository retirement did not change exactly one active row");
          }
        }
        return repository.status;
      });
    } catch (error) {
      return expectedFailure(error);
    }
    if (status === "deleted") return { ok: true as const, deleted: false };

    const authority = { ...identity, repositoryId: request.repositoryId, incarnation: request.incarnation };
    const stub = this.env.REPOSITORIES.get(
      this.env.REPOSITORIES.idFromString(request.repositoryId),
    );
    const retired = await stub.retireRepository(authority);
    if (!retired.ok) return retired;
    this.finalizeRepositoryRetirement(identity.userId, request.repositoryId);
    return { ok: true as const, deleted: true };
  }

  private async recoverRetiringRepositories(machineId: string, repositories: RepositoryRow[]) {
    for (const repository of repositories) {
      const authority = {
        userId: repository.user_id,
        machineId,
        repositoryId: repository.repository_id,
        incarnation: repository.incarnation,
      };
      const stub = this.env.REPOSITORIES.get(
        this.env.REPOSITORIES.idFromString(repository.repository_id),
      );
      const retired = await stub.retireRepository(authority);
      if (!retired.ok) {
        throw new Error(`repository retirement recovery failed: ${retired.error}`);
      }
      this.finalizeRepositoryRetirement(repository.user_id, repository.repository_id);
    }
  }

  private finalizeRepositoryRetirement(userId: string, repositoryId: string) {
    this.ctx.storage.transactionSync(() => {
      const repository = this.repositoryById(userId, repositoryId);
      if (repository?.status === "deleted") return;
      if (repository?.status !== "retiring") {
        throw new Error("only a retiring repository can become deleted");
      }
      const changed = this.ctx.storage.sql
        .exec<{ repository_id: string }>(
          `UPDATE repositories SET status = 'deleted', deleted_at_ms = ?
           WHERE user_id = ? AND repository_id = ? AND status = 'retiring'
           RETURNING repository_id`,
          Date.now(),
          userId,
          repositoryId,
        )
        .toArray();
      if (changed.length !== 1) {
        throw new Error("repository deletion did not change exactly one retiring row");
      }
    });
  }

  private repositoryById(userId: string, repositoryId: string): RepositoryRow | undefined {
    return this.ctx.storage.sql
      .exec<RepositoryRow>(
        `SELECT repository_id, user_id, name, incarnation, status FROM repositories
         WHERE user_id = ? AND repository_id = ?`,
        userId,
        repositoryId,
      )
      .toArray()[0];
  }
}

function initializeControlPlaneSchema(sql: SqlStorage) {
  sql.exec(`
    CREATE TABLE IF NOT EXISTS _sql_schema_migrations (
      id INTEGER PRIMARY KEY,
      applied_at_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS repositories (
      repository_id TEXT PRIMARY KEY,
      user_id TEXT NOT NULL,
      name TEXT NOT NULL,
      incarnation TEXT NOT NULL,
      status TEXT NOT NULL CHECK (status IN ('provisional', 'active', 'retiring', 'deleted')),
      created_at_ms INTEGER NOT NULL,
      deleted_at_ms INTEGER
    ) WITHOUT ROWID;
    CREATE UNIQUE INDEX IF NOT EXISTS active_repository_name
      ON repositories (user_id, name) WHERE status != 'deleted';
    CREATE TABLE IF NOT EXISTS repository_creation_receipts (
      user_id TEXT NOT NULL,
      idempotency_key TEXT NOT NULL,
      name TEXT NOT NULL,
      repository_id TEXT NOT NULL,
      incarnation TEXT NOT NULL,
      PRIMARY KEY (user_id, idempotency_key)
    ) WITHOUT ROWID;
    INSERT OR IGNORE INTO _sql_schema_migrations VALUES (2, ${Date.now()});
  `);
}

function decodeIdentity(value: unknown): AuthenticatedPrincipal {
  if (!isRecord(value)) throw new Error("authenticated identity is invalid");
  requireExactKeys(value, ["userId", "machineId"]);
  return {
    userId: requireId(value.userId, "user ID"),
    machineId: requireMachineId(value.machineId),
  };
}

function decodeCreateRepository(value: unknown) {
  if (!isRecord(value)) throw new Error("repository creation request must be a JSON object");
  requireExactKeys(value, ["name", "idempotencyKey"]);
  if (typeof value.idempotencyKey !== "string" || !IDEMPOTENCY_KEY_PATTERN.test(value.idempotencyKey)) {
    throw new Error("idempotencyKey must be 32 lowercase hex characters");
  }
  return { name: requireRepositoryName(value.name), idempotencyKey: value.idempotencyKey };
}

function decodeDeleteRepository(value: unknown) {
  if (!isRecord(value)) throw new Error("repository deletion request must be a JSON object");
  requireExactKeys(value, ["name", "repositoryId", "incarnation"]);
  return {
    name: requireRepositoryName(value.name),
    repositoryId: requireRepositoryId(value.repositoryId),
    incarnation: requireIncarnation(value.incarnation),
  };
}

function requireId(value: unknown, label: string): string {
  if (typeof value !== "string" || !ID_PATTERN.test(value)) {
    throw new Error(`${label} is invalid`);
  }
  return value;
}

function requireMachineId(value: unknown): string {
  if (typeof value !== "string" || !MACHINE_ID_PATTERN.test(value)) {
    throw new Error("machine ID must be 32 lowercase hex characters");
  }
  return value;
}

function requireRepositoryName(value: unknown): string {
  if (typeof value !== "string" || !REPOSITORY_NAME_PATTERN.test(value)) {
    throw new Error("invalid repository name");
  }
  return value;
}

function requireRepositoryId(value: unknown): string {
  if (typeof value !== "string" || !REPOSITORY_ID_PATTERN.test(value)) {
    throw new Error("invalid repository ID");
  }
  return value;
}

function requireIncarnation(value: unknown): string {
  if (typeof value !== "string" || !INCARNATION_PATTERN.test(value)) {
    throw new Error("incarnation must be 32 lowercase hex characters");
  }
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requireExactKeys(value: Record<string, unknown>, expected: string[]) {
  const keys = Object.keys(value).sort();
  const sortedExpected = [...expected].sort();
  if (keys.length !== sortedExpected.length || keys.some((key, index) => key !== sortedExpected[index])) {
    throw new Error(`request fields must be exactly ${expected.join(", ")}`);
  }
}

function randomHex(bytes: number): string {
  return toHex(crypto.getRandomValues(new Uint8Array(bytes)));
}

function expectedFailure(error: unknown) {
  if (error instanceof ControlPlaneError) return failure(error, error.status);
  throw error;
}

function failure(error: unknown, status: number) {
  const code = error instanceof ControlPlaneError ? error.code : undefined;
  return {
    ok: false as const,
    status,
    error: error instanceof Error ? error.message : String(error),
    ...(code === undefined ? {} : { code }),
  };
}

function creationSuccess(authority: RepositoryAuthority, name: string) {
  return {
    ok: true as const,
    repositoryId: authority.repositoryId,
    incarnation: authority.incarnation,
    name,
  };
}
