import { DurableObject } from "cloudflare:workers";
import { HeadStore } from "./head_store";
import { Kernel } from "./kernel";
import { PackStore } from "./pack_store";
import { ProjectionStore } from "./projection_store";
import { initializeSchema } from "./schema";

export interface Env {
  REPOSITORIES: DurableObjectNamespace<Repository>;
  DEVSPACE_TOKEN: string;
}

export class Repository extends DurableObject<Env> {
  private readonly heads: HeadStore;
  private readonly packs: PackStore;
  private readonly projection: ProjectionStore;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    const sql = this.ctx.storage.sql;
    initializeSchema(sql);
    const kernel = new Kernel();
    this.heads = new HeadStore(this.ctx, sql, kernel);
    this.packs = new PackStore(this.ctx, sql, kernel, this.heads);
    this.projection = new ProjectionStore(this.ctx, sql, kernel);
  }

  putPackManifest(packId: string, bytes: Uint8Array) {
    return this.packs.putPackManifest(packId, bytes);
  }

  putPackChunk(packId: string, position: number, bytes: Uint8Array) {
    return this.packs.putPackChunk(packId, position, bytes);
  }

  installPack(packId: string) {
    return this.packs.installPack(packId);
  }

  countObjects() {
    return this.packs.countObjects();
  }

  countInstalledPacks() {
    return this.packs.countInstalledPacks();
  }

  inventoryObjects(value: unknown) {
    return this.packs.inventoryObjects(value);
  }

  initializeRepository(incarnationValue: unknown) {
    return this.heads.initialize(incarnationValue);
  }

  getHeads(incarnationValue: unknown) {
    return this.heads.get(incarnationValue);
  }

  transactHeads(value: unknown) {
    return this.heads.transact(value);
  }

  listInstalledPacks(incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    return this.packs.listInstalledPacks(incarnationValue, afterValue, throughValue);
  }

  getInstalledPackManifest(packId: string, incarnationValue: unknown) {
    return this.packs.getInstalledPackManifest(packId, incarnationValue);
  }

  getInstalledPackChunk(packId: string, position: number, incarnationValue: unknown) {
    return this.packs.getInstalledPackChunk(packId, position, incarnationValue);
  }

  getProjection(incarnationValue: unknown, afterValue: unknown, throughValue: unknown) {
    return this.projection.get(incarnationValue, afterValue, throughValue);
  }

  mutateHiddenPolicy(value: unknown) {
    return this.projection.mutatePolicy(value);
  }

  beginProjectionPush(value: unknown) {
    return this.projection.begin(value);
  }

  claimProjectionPush(batchId: unknown, value: unknown) {
    return this.projection.claim(batchId, value);
  }

  getProjectionPushReplay(batchId: unknown, incarnationValue: unknown) {
    return this.projection.replay(batchId, incarnationValue);
  }

  confirmProjectionPush(batchId: unknown, value: unknown) {
    return this.projection.confirm(batchId, value);
  }

  recoverProjectionPush(batchId: unknown, value: unknown) {
    return this.projection.recover(batchId, value);
  }
}
