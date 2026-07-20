import { DurableObject } from "cloudflare:workers";
import { RepositoryAuthority } from "./control_plane";
import { HeadStore } from "./head_store";
import { Kernel, equalBytes, exactBuffer } from "./kernel";
import { PackStore } from "./pack_store";
import { ProjectionStore } from "./projection_store";
import { initializeSchema } from "./schema";

interface AuthorityRow extends Record<string, SqlStorageValue> {
  incarnation: ArrayBuffer;
  user_id: string | null;
  repository_id: string | null;
  retired: number;
}

export class Repository extends DurableObject<Env> {
  private readonly heads: HeadStore;
  private readonly packs: PackStore;
  private readonly projection: ProjectionStore;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    const sql = this.ctx.storage.sql;
    this.ctx.blockConcurrencyWhile(async () =>
      this.ctx.storage.transactionSync(() => initializeSchema(sql)),
    );
    const kernel = new Kernel();
    this.heads = new HeadStore(this.ctx, sql, kernel);
    this.packs = new PackStore(this.ctx, sql, kernel, this.heads);
    this.projection = new ProjectionStore(this.ctx, sql, kernel);
  }

  putPackManifest(authority: RepositoryAuthority, packId: string, bytes: Uint8Array) {
    return this.withAuthority(authority, () => this.packs.putPackManifest(packId, bytes));
  }

  putPackChunk(authority: RepositoryAuthority, packId: string, position: number, bytes: Uint8Array) {
    return this.withAuthority(authority, () => this.packs.putPackChunk(packId, position, bytes));
  }

  installPack(authority: RepositoryAuthority, packId: string) {
    return this.withAuthority(authority, () => this.packs.installPack(packId));
  }

  countObjects() {
    return this.packs.countObjects();
  }

  countInstalledPacks() {
    return this.packs.countInstalledPacks();
  }

  inventoryObjects(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.packs.inventoryObjects(value));
  }

  initializeRepository(authority: RepositoryAuthority) {
    return this.heads.initialize(authority);
  }

  retireRepository(authority: RepositoryAuthority) {
    try {
      return this.ctx.storage.transactionSync(() => {
        const state = this.ctx.storage.sql
          .exec<AuthorityRow>(
            `SELECT incarnation, user_id, repository_id, retired
             FROM repository_state WHERE singleton = 1`,
          )
          .toArray()[0];
        if (state === undefined) {
          this.ctx.storage.sql.exec(
            `INSERT INTO repository_state
             (singleton, incarnation, user_id, repository_id, retired, cursor,
              receipt_count, receipt_head_count)
             VALUES (1, ?, ?, ?, 1, 0, 0, 0)`,
            exactBuffer(hexBytes(authority.incarnation)),
            authority.userId,
            authority.repositoryId,
          );
          return { ok: true as const, retired: true };
        }
        this.requireAuthority(authority, true);
        if (state.retired === 0) {
          const changed = this.ctx.storage.sql
            .exec<{ singleton: number }>(
              `UPDATE repository_state SET retired = 1
               WHERE singleton = 1 AND retired = 0 RETURNING singleton`,
            )
            .toArray();
          if (changed.length !== 1) {
            throw new Error("repository retirement did not change exactly one active state row");
          }
        }
        return { ok: true as const, retired: true };
      });
    } catch (error) {
      return authorityFailure(error);
    }
  }

  getHeads(authority: RepositoryAuthority, incarnationValue: unknown) {
    return this.withAuthority(authority, () => this.heads.get(incarnationValue));
  }

  transactHeads(authority: RepositoryAuthority, value: unknown) {
    return this.withAuthority(authority, () => this.heads.transact(value));
  }

  listInstalledPacks(authority: RepositoryAuthority, incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    return this.withAuthority(authority, () =>
      this.packs.listInstalledPacks(incarnationValue, afterValue, throughValue),
    );
  }

  getInstalledPackManifest(authority: RepositoryAuthority, packId: string, incarnationValue: unknown) {
    return this.withAuthority(authority, () =>
      this.packs.getInstalledPackManifest(packId, incarnationValue),
    );
  }

  getInstalledPackChunk(authority: RepositoryAuthority, packId: string, position: number, incarnationValue: unknown) {
    return this.withAuthority(authority, () =>
      this.packs.getInstalledPackChunk(packId, position, incarnationValue),
    );
  }

  getProjection(authority: RepositoryAuthority, incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
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

  claimProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.claim(batchId, value, authority.machineId),
    );
  }

  getProjectionPushReplay(authority: RepositoryAuthority, batchId: unknown, incarnationValue: unknown) {
    return this.withAuthority(authority, () => this.projection.replay(batchId, incarnationValue));
  }

  confirmProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.confirm(batchId, value, authority.machineId),
    );
  }

  recoverProjectionPush(authority: RepositoryAuthority, batchId: unknown, value: unknown) {
    return this.withAuthority(authority, () =>
      this.projection.recover(batchId, value, authority.machineId),
    );
  }

  private withAuthority<T>(authority: RepositoryAuthority, operation: () => T) {
    try {
      this.requireAuthority(authority, false);
    } catch (error) {
      return authorityFailure(error);
    }
    return operation();
  }

  private requireAuthority(authority: RepositoryAuthority, allowRetired: boolean) {
    const state = this.ctx.storage.sql
      .exec<AuthorityRow>(
        `SELECT incarnation, user_id, repository_id, retired
         FROM repository_state WHERE singleton = 1`,
      )
      .toArray()[0];
    if (
      state === undefined ||
      state.user_id !== authority.userId ||
      state.repository_id !== authority.repositoryId ||
      !equalBytes(new Uint8Array(state.incarnation), hexBytes(authority.incarnation)) ||
      (!allowRetired && state.retired !== 0)
    ) {
      throw new Error("repository authority is stale");
    }
  }
}

function authorityFailure(error: unknown) {
  return {
    ok: false as const,
    status: 409,
    error: error instanceof Error ? error.message : "repository authority is stale",
  };
}

function hexBytes(value: string): Uint8Array {
  if (!/^[0-9a-f]{32}$/.test(value)) throw new Error("repository authority is invalid");
  return Uint8Array.from({ length: 16 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}
