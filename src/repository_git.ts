import { DurableObject } from "cloudflare:workers";
import type { RepositoryAuthority } from "./control_plane";
import { KernelGit, equalGitBytes, exactGitBuffer } from "./kernel_git";
import { OpGitStore } from "./op_git_store";
import { GitPackStore } from "./pack_git_store";
import { ProjectionGitStore } from "./projection_git_store";
import { initializeGitSchema } from "./schema_git";

class RepositoryGitAuthorityError extends Error {
  constructor(
    message: string,
    readonly code: "repository-retired" | "repository-authority-stale",
  ) {
    super(message);
  }
}

interface AuthorityRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  user_id: string;
  repository_id: string;
  retired: number;
}

export class RepositoryGit extends DurableObject<Env> {
  private readonly packs: GitPackStore;
  private readonly ops: OpGitStore;
  private readonly projection: ProjectionGitStore;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    const sql = this.ctx.storage.sql;
    this.ctx.blockConcurrencyWhile(async () =>
      this.ctx.storage.transactionSync(() => initializeGitSchema(sql)),
    );
    const kernel = new KernelGit();
    this.packs = new GitPackStore(this.ctx, sql, kernel);
    this.ops = new OpGitStore(this.ctx, sql, kernel);
    this.projection = new ProjectionGitStore(this.ctx, sql, kernel);
  }

  initializeRepository(authority: RepositoryAuthority) {
    try {
      return this.ctx.storage.transactionSync(() => {
        const state = this.authorityState();
        if (state === undefined) {
          this.ctx.storage.sql.exec(
            `INSERT INTO repository_state
             (singleton, incarnation, user_id, repository_id, retired)
             VALUES (1, ?, ?, ?, 0)`,
            exactGitBuffer(incarnationBytes(authority.incarnation)),
            authority.userId,
            authority.repositoryId,
          );
          return { ok: true as const, initialized: true };
        }
        this.requireAuthority(authority);
        return { ok: true as const, initialized: false };
      });
    } catch (error) {
      return authorityFailure(error);
    }
  }

  async retireRepository(authority: RepositoryAuthority) {
    try {
      const state = this.authorityState();
      if (state !== undefined) this.requireAuthority(authority);
      await this.ctx.storage.deleteAll();
      return { ok: true as const, retired: true };
    } catch (error) {
      return authorityFailure(error);
    }
  }

  putPackManifest(authority: RepositoryAuthority, packId: string, bytes: Uint8Array) {
    return this.withAuthority(authority, () => this.packs.putPackManifest(packId, bytes));
  }

  putPackChunk(authority: RepositoryAuthority, packId: string, position: number, bytes: Uint8Array) {
    return this.withAuthority(authority, () =>
      this.packs.putPackChunk(packId, position, bytes),
    );
  }

  installPack(authority: RepositoryAuthority, packId: string) {
    return this.withAuthority(authority, () => this.packs.installPack(packId));
  }

  listInstalledPacks(authority: RepositoryAuthority, afterValue: unknown, throughValue: unknown) {
    return this.withAuthority(authority, () =>
      this.packs.listInstalledPacks(afterValue, throughValue),
    );
  }

  getInstalledPackManifest(authority: RepositoryAuthority, packId: string) {
    return this.withAuthority(authority, () => this.packs.getInstalledPackManifest(packId));
  }

  getInstalledPackChunk(authority: RepositoryAuthority, packId: string, position: number) {
    return this.withAuthority(authority, () =>
      this.packs.getInstalledPackChunk(packId, position),
    );
  }

  putOpObject(
    authority: RepositoryAuthority,
    kind: "view" | "operation",
    id: string,
    bytes: Uint8Array,
  ) {
    return this.withAuthority(authority, () => this.ops.put(kind, id, bytes));
  }

  getOpObject(authority: RepositoryAuthority, kind: "view" | "operation", id: string) {
    return this.withAuthority(authority, () => this.ops.get(kind, id));
  }

  inventoryOpObjects(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.ops.inventory(value));
  }

  getOpHeads(authority: RepositoryAuthority) {
    return this.withAuthority(authority, () => this.ops.getHeads(authority.incarnation));
  }

  transactOpHeads(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.ops.transactHeads(value));
  }

  getProjection(
    authority: RepositoryAuthority,
    incarnationValue: unknown,
    afterValue: unknown,
    throughValue: unknown,
  ) {
    return this.withAuthority(authority, () =>
      this.projection.get(incarnationValue, afterValue, throughValue),
    );
  }

  setRemote(authority: RepositoryAuthority, name: unknown, value: unknown) {
    return this.withAuthority(authority, () => this.projection.setRemote(name, value));
  }

  listRemotes(authority: RepositoryAuthority, incarnationValue: unknown) {
    return this.withAuthority(authority, () => this.projection.listRemotes(incarnationValue));
  }

  beginProjectionPush(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.projection.begin(value, authority.machineId));
  }

  recordProjectionFetch(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.recordFetch(value, authority.machineId),
    );
  }

  claimProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.claim(batchId, value, authority.machineId),
    );
  }

  getProjectionPushReplay(
    authority: RepositoryAuthority,
    batchId: unknown,
    incarnationValue: unknown,
  ) {
    return this.withAuthority(authority, () =>
      this.projection.replay(batchId, incarnationValue),
    );
  }

  recoverProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.recover(batchId, value, authority.machineId),
    );
  }

  countObjects() {
    return this.packs.countObjects();
  }

  countObjectReferences() {
    return this.packs.countObjectReferences();
  }

  countInstalledPacks() {
    return this.packs.countInstalledPacks();
  }

  countQuarantinedPacks() {
    return this.packs.countQuarantinedPacks();
  }

  countOpObjects() {
    return this.ops.countObjects();
  }

  private withAuthority<T>(authority: RepositoryAuthority, operation: () => T) {
    try {
      this.requireAuthority(authority);
    } catch (error) {
      return authorityFailure(error);
    }
    return operation();
  }

  private authorityState(): AuthorityRow | undefined {
    return this.ctx.storage.sql
      .exec<AuthorityRow>(
        `SELECT incarnation, user_id, repository_id, retired
         FROM repository_state WHERE singleton = 1`,
      )
      .toArray()[0];
  }

  private requireAuthority(authority: RepositoryAuthority) {
    const state = this.authorityState();
    if (
      state === undefined ||
      state.user_id !== authority.userId ||
      state.repository_id !== authority.repositoryId ||
      !equalGitBytes(new Uint8Array(state.incarnation), incarnationBytes(authority.incarnation))
    ) {
      throw new RepositoryGitAuthorityError(
        "repository authority is stale",
        "repository-authority-stale",
      );
    }
    if (state.retired !== 0) {
      throw new RepositoryGitAuthorityError("repository was deleted", "repository-retired");
    }
  }
}

function authorityFailure(error: unknown) {
  return {
    ok: false as const,
    status: 409,
    error: error instanceof Error ? error.message : "repository authority is stale",
    code:
      error instanceof RepositoryGitAuthorityError
        ? error.code
        : "repository-authority-stale",
  };
}

function incarnationBytes(value: string): Uint8Array {
  if (!/^[0-9a-f]{32}$/.test(value)) throw new Error("repository authority is invalid");
  return Uint8Array.from({ length: 16 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}
