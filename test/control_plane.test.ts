import { env, exports } from "cloudflare:workers";
import { evictDurableObject, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";

const packId = "00".repeat(64);

describe("cloud identity and repository directory", () => {
  it("authenticates 2 machines with the shared development secret", async () => {
    const firstMachine = "11".repeat(16);
    const secondMachine = "22".repeat(16);
    const repository = await createRepository(firstMachine, "shared-machines", "11".repeat(16));

    const control = env.CONTROL_PLANE.getByName("directory");
    expect(
      await control.authorizeRepository(
        { userId: env.DEVSPACE_DEVELOPMENT_USER_ID, machineId: firstMachine },
        repository.repositoryId,
        repository.incarnation,
      ),
    ).toMatchObject({ ok: true, authority: { machineId: firstMachine } });
    expect(
      await control.authorizeRepository(
        { userId: env.DEVSPACE_DEVELOPMENT_USER_ID, machineId: secondMachine },
        repository.repositoryId,
        repository.incarnation,
      ),
    ).toMatchObject({ ok: true, authority: { machineId: secondMachine } });
    await evictDurableObject(control);

    expect(await (await apiRequest(firstMachine, "/repositories/shared-machines")).json()).toEqual(
      repository,
    );
    expect(await (await apiRequest(secondMachine, "/repositories/shared-machines")).json()).toEqual(
      repository,
    );
  });

  it("keeps synthetic principals isolated below the HTTP auth adapter", async () => {
    const control = env.CONTROL_PLANE.getByName("directory");
    const owner = { userId: "owner-user", machineId: "31".repeat(16) };
    const stranger = { userId: "stranger-user", machineId: "32".repeat(16) };
    const created = await control.createRepository(owner, {
      name: "private-repository",
      idempotencyKey: "31".repeat(16),
    });
    if (!created.ok) throw new Error(created.error);

    expect(await control.resolveRepository(stranger, "private-repository")).toEqual({
      ok: false,
      status: 404,
      error: "repository not found",
      code: "repository-not-found",
    });
    expect(
      await control.authorizeRepository(stranger, created.repositoryId, created.incarnation),
    ).toMatchObject({
      ok: false,
      status: 404,
      code: "repository-not-found",
    });
    const repository = env.REPOSITORIES.getByName(created.repositoryId);
    expect(
      await repository.putPackManifest(
        {
          ...stranger,
          repositoryId: created.repositoryId,
          incarnation: created.incarnation,
          creationNonce: "00".repeat(16),
        },
        packId,
        new Uint8Array(),
      ),
    ).toMatchObject({
      ok: false,
      status: 409,
      error: "repository authority is stale",
      code: "repository-authority-stale",
    });
  });

  it("denies missing and invalid secrets and rejects malformed machine IDs", async () => {
    const missing = await exports.default.fetch(
      new Request("https://example.com/repositories/test", {
        headers: { "x-devspace-machine-id": "33".repeat(16) },
      }),
    );
    expect(missing.status).toBe(401);
    expect(await missing.json()).toEqual({ error: "unauthorized", code: "unauthorized" });

    const invalid = await exports.default.fetch(
      new Request("https://example.com/repositories/test", {
        headers: {
          authorization: "Bearer invalid",
          "x-devspace-machine-id": "33".repeat(16),
        },
      }),
    );
    expect(invalid.status).toBe(401);
    expect(await invalid.json()).toEqual({ error: "unauthorized", code: "unauthorized" });

    const malformedMachine = await apiRequest("not-a-machine", "/repositories/test");
    expect(malformedMachine.status).toBe(400);
    expect(await malformedMachine.json()).toEqual({
      error: "invalid machine ID",
      code: "invalid-machine-id",
    });
  });

  it("does not let request fields or headers select a user", async () => {
    const machineId = "34".repeat(16);
    const created = await apiRequest(machineId, "/repositories", {
      method: "POST",
      headers: { "content-type": "application/json", "x-devspace-user-id": "attacker" },
      body: JSON.stringify({
        name: "header-identity-injection",
        idempotencyKey: "34".repeat(16),
      }),
    });
    expect(created.status).toBe(200);
    const control = env.CONTROL_PLANE.getByName("directory");
    expect(
      await control.resolveRepository(
        { userId: env.DEVSPACE_DEVELOPMENT_USER_ID, machineId },
        "header-identity-injection",
      ),
    ).toMatchObject({ ok: true });
    expect(
      await control.resolveRepository(
        { userId: "attacker", machineId },
        "header-identity-injection",
      ),
    ).toEqual({
      ok: false,
      status: 404,
      error: "repository not found",
      code: "repository-not-found",
    });

    const injectedBody = await apiRequest(machineId, "/repositories", {
      method: "POST",
      headers: { "content-type": "application/json", "x-devspace-user-id": "attacker" },
      body: JSON.stringify({
        name: "body-identity-injection",
        idempotencyKey: "35".repeat(16),
        userId: "attacker",
      }),
    });
    expect(injectedBody.status).toBe(400);
    expect(await injectedBody.json()).toMatchObject({ code: "invalid-control-plane-request" });
  });

  it("codes malformed repository names and request bodies", async () => {
    const machineId = "36".repeat(16);

    const invalidName = await apiRequest(machineId, "/repositories/Invalid");
    expect({ status: invalidName.status, body: await invalidName.json() }).toMatchObject({
      status: 400,
      body: { code: "invalid-repository-name" },
    });

    const invalidCreation = await apiRequest(machineId, "/repositories", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{",
    });
    expect({ status: invalidCreation.status, body: await invalidCreation.json() }).toMatchObject({
      status: 400,
      body: { code: "invalid-repository-creation-request" },
    });

    const invalidRename = await apiRequest(machineId, "/repositories/rename-source", {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: "{",
    });
    expect({ status: invalidRename.status, body: await invalidRename.json() }).toMatchObject({
      status: 400,
      body: { code: "invalid-repository-rename-request" },
    });

    const invalidDeletion = await apiRequest(machineId, "/repositories/delete-source", {
      method: "DELETE",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ repositoryId: "not-an-id", incarnation: "not-an-incarnation" }),
    });
    expect({ status: invalidDeletion.status, body: await invalidDeletion.json() }).toMatchObject({
      status: 400,
      body: { code: "invalid-control-plane-request" },
    });
  });

  it("replays lost create responses and rejects idempotency-key reuse", async () => {
    const machineId = "41".repeat(16);
    const idempotencyKey = "41".repeat(16);
    const first = await createRepository(machineId, "retry-safe", idempotencyKey);
    await evictDurableObject(env.CONTROL_PLANE.getByName("directory"));
    expect(await createRepository(machineId, "retry-safe", idempotencyKey)).toEqual(first);

    const conflict = await apiRequest(machineId, "/repositories", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "different-request", idempotencyKey }),
    });
    expect(conflict.status).toBe(409);
    expect(await conflict.json()).toEqual({
      error: "idempotency key was already used for a different repository request",
      code: "idempotency-key-reused",
    });

    const nameConflict = await apiRequest(machineId, "/repositories", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "retry-safe", idempotencyKey: "45".repeat(16) }),
    });
    expect(nameConflict.status).toBe(409);
    expect(await nameConflict.json()).toEqual({
      error: "repository name is already in use",
      code: "repository-name-taken",
    });
  });

  it("lists only active repositories and requires authentication", async () => {
    const machineId = "47".repeat(16);
    const active = await createRepository(machineId, "listed-active", "47".repeat(16));
    const retired = await createRepository(machineId, "listed-retired", "48".repeat(16));
    const control = env.CONTROL_PLANE.getByName("directory");
    expect(
      await control.beginTestRepositoryRetirement(
        { userId: env.DEVSPACE_DEVELOPMENT_USER_ID, machineId },
        retired.repositoryId,
      ),
    ).toEqual({ ok: true, retiring: true });

    const response = await apiRequest(machineId, "/repositories");
    expect(response.status).toBe(200);
    const body = (await response.json()) as { repositories: Array<{ name: string }> };
    expect(body.repositories).toEqual(expect.arrayContaining([active]));
    expect(body.repositories.map((repository) => repository.name)).not.toContain(
      "listed-retired",
    );
    const unauthorized = await exports.default.fetch(
      new Request("https://example.com/repositories"),
    );
    expect(unauthorized.status).toBe(401);
    expect(await unauthorized.json()).toEqual({ error: "unauthorized", code: "unauthorized" });
  });

  it("renames repositories, rejects collisions, and frees the old name", async () => {
    const machineId = "49".repeat(16);
    const renamed = await createRepository(machineId, "rename-source", "49".repeat(16));
    await createRepository(machineId, "rename-taken", "4a".repeat(16));

    const collision = await apiRequest(machineId, "/repositories/rename-source", {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ newName: "rename-taken" }),
    });
    expect(collision.status).toBe(409);
    expect(await collision.json()).toEqual({
      error: "repository name is already in use",
      code: "repository-name-taken",
    });

    const response = await apiRequest(machineId, "/repositories/rename-source", {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ newName: "rename-target" }),
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ...renamed, name: "rename-target" });
    expect((await apiRequest(machineId, "/repositories/rename-source")).status).toBe(404);

    const noOp = await apiRequest(machineId, "/repositories/rename-target", {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ newName: "rename-target" }),
    });
    expect(noOp.status).toBe(200);
    expect(await noOp.json()).toEqual({ ...renamed, name: "rename-target" });

    const replacement = await createRepository(machineId, "rename-source", "4b".repeat(16));
    expect(replacement.repositoryId).not.toBe(renamed.repositoryId);
  });

  it("recovers a persisted retirement before recreating its name", async () => {
    const userId = env.DEVSPACE_DEVELOPMENT_USER_ID;
    const machineId = "42".repeat(16);
    const idempotencyKey = "42".repeat(16);
    const repository = await createRepository(machineId, "retiring-replay", idempotencyKey);
    const control = env.CONTROL_PLANE.getByName("directory");
    const staleAuthorization = await control.authorizeRepository(
      { userId, machineId },
      repository.repositoryId,
      repository.incarnation,
    );
    if (!staleAuthorization.ok) throw new Error(staleAuthorization.error);
    expect(
      await control.beginTestRepositoryRetirement(
        { userId, machineId },
        repository.repositoryId,
      ),
    ).toEqual({ ok: true, retiring: true });
    await evictDurableObject(control);

    const replacement = await createRepository(machineId, "retiring-replay", "44".repeat(16));
    expect(replacement.repositoryId).not.toBe(repository.repositoryId);
    expect(replacement.incarnation).not.toBe(repository.incarnation);

    const replay = await apiRequest(machineId, "/repositories", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ name: "retiring-replay", idempotencyKey }),
    });
    expect(replay.status).toBe(409);
    expect(await replay.json()).toEqual({
      error: "repository created by this request was retired",
      code: "creation-retired",
    });
    expect(await (await apiRequest(machineId, "/repositories/retiring-replay")).json()).toEqual(
      replacement,
    );

    const repositoryStub = env.REPOSITORIES.getByName(repository.repositoryId);
    await evictDurableObject(repositoryStub);
    expect(
      await repositoryStub.initializeRepository(staleAuthorization.authority),
    ).toMatchObject({
      ok: false,
      status: 409,
      code: "repository-authority-stale",
    });
    expect(
      await repositoryStub.putPackManifest(
        staleAuthorization.authority,
        packId,
        new Uint8Array(),
      ),
    ).toMatchObject({ ok: false, status: 409, code: "repository-authority-stale" });
  });

  it("reports a replay whose repository is concurrently retiring", async () => {
    const identity = {
      userId: env.DEVSPACE_DEVELOPMENT_USER_ID,
      machineId: "46".repeat(16),
    };
    const idempotencyKey = "46".repeat(16);
    const repository = await createRepository(
      identity.machineId,
      "concurrent-retiring-replay",
      idempotencyKey,
    );
    const control = env.CONTROL_PLANE.getByName("directory");
    await runInDurableObject(control, (instance, state) => {
      state.storage.sql.exec(
        "UPDATE repositories SET status = 'retiring' WHERE repository_id = ?",
        repository.repositoryId,
      );
      const testControl = instance as unknown as {
        recoverRetiringRepositories?: () => Promise<void>;
      };
      testControl.recoverRetiringRepositories = async () => {};
    });
    try {
      const replay = await apiRequest(identity.machineId, "/repositories", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ name: "concurrent-retiring-replay", idempotencyKey }),
      });
      expect(replay.status).toBe(409);
      expect(await replay.json()).toEqual({
        error: "repository created by this request is retiring",
        code: "creation-retiring",
      });
    } finally {
      await runInDurableObject(control, (instance) => {
        const testControl = instance as unknown as {
          recoverRetiringRepositories?: () => Promise<void>;
        };
        delete testControl.recoverRetiringRepositories;
      });
    }
  });

  it("retires a deleted incarnation before recreating the name", async () => {
    const machineId = "51".repeat(16);
    const first = await createRepository(machineId, "replaceable", "51".repeat(16));
    const firstStub = env.REPOSITORIES.getByName(first.repositoryId);
    const authorized = await env.CONTROL_PLANE.getByName("directory").authorizeRepository(
      { userId: env.DEVSPACE_DEVELOPMENT_USER_ID, machineId },
      first.repositoryId,
      first.incarnation,
    );
    if (!authorized.ok) throw new Error(authorized.error);
    const firstAuthority = authorized.authority;
    expect(await firstStub.initializeRepository(firstAuthority)).toMatchObject({ ok: true });
    await runInDurableObject(firstStub, (_instance, state) => {
      state.storage.sql.exec(
        "INSERT INTO objects (kind, id, bytes) VALUES (?, ?, ?)",
        1,
        new Uint8Array(20).fill(1),
        new Uint8Array([2]),
      );
    });
    await evictDurableObject(firstStub);

    const deleted = await apiRequest(machineId, "/repositories/replaceable", {
      method: "DELETE",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        repositoryId: first.repositoryId,
        incarnation: first.incarnation,
      }),
    });
    expect(deleted.status).toBe(200);
    expect(await deleted.json()).toEqual({ deleted: true });
    await evictDurableObject(firstStub);
    expect(await firstStub.countObjects()).toBe(0);
    await runInDurableObject(firstStub, (_instance, state) => {
      expect(
        state.storage.sql
          .exec<{ count: number }>("SELECT count(*) AS count FROM repository_state")
          .one().count,
      ).toBe(0);
    });
    expect(
      await firstStub.putPackManifest(
        firstAuthority,
        packId,
        new Uint8Array(),
      ),
    ).toMatchObject({
      ok: false,
      status: 409,
      code: "repository-authority-stale",
    });

    const second = await createRepository(machineId, "replaceable", "52".repeat(16));
    expect(second.repositoryId).not.toBe(first.repositoryId);
    expect(second.incarnation).not.toBe(first.incarnation);

    const staleHeaders = { "x-devspace-incarnation": first.incarnation };
    expect(
      (
        await apiRequest(
          machineId,
          `/repositories/${first.repositoryId}/git/ops/heads`,
          { headers: staleHeaders },
        )
      ).status,
    ).toBe(404);
    expect(
      (
        await apiRequest(machineId, `/repositories/${first.repositoryId}/git/ops/heads/transactions`, {
          method: "POST",
          headers: { ...staleHeaders, "content-type": "application/json" },
          body: "{}",
        })
      ).status,
    ).toBe(404);
    expect(
      (
        await apiRequest(
          machineId,
          `/repositories/${second.repositoryId}/git/ops/heads`,
          { headers: staleHeaders },
        )
      ).status,
    ).toBe(404);

    const current = await apiRequest(
      machineId,
      `/repositories/${second.repositoryId}/git/ops/heads`,
      { headers: { "x-devspace-incarnation": second.incarnation } },
    );
    expect(current.status).toBe(200);
  });

  it("codes deletion attempts while repository creation is provisional", async () => {
    const machineId = "53".repeat(16);
    const repository = await createRepository(machineId, "provisional-delete", "53".repeat(16));
    const control = env.CONTROL_PLANE.getByName("directory");
    await runInDurableObject(control, (_instance, state) => {
      state.storage.sql.exec(
        "UPDATE repositories SET status = 'provisional' WHERE repository_id = ?",
        repository.repositoryId,
      );
    });

    const response = await apiRequest(machineId, "/repositories/provisional-delete", {
      method: "DELETE",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        repositoryId: repository.repositoryId,
        incarnation: repository.incarnation,
      }),
    });
    expect({ status: response.status, body: await response.json() }).toMatchObject({
      status: 409,
      body: { code: "repository-creation-provisional" },
    });
  });
});

async function apiRequest(machineId: string, path: string, init: RequestInit = {}) {
  return exports.default.fetch(
    new Request(`https://example.com${path}`, {
      ...init,
      headers: {
        authorization: `Bearer ${env.DEVSPACE_SHARED_SECRET}`,
        "x-devspace-machine-id": machineId,
        ...init.headers,
      },
    }),
  );
}

async function createRepository(machineId: string, name: string, idempotencyKey: string) {
  const response = await apiRequest(machineId, "/repositories", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name, idempotencyKey }),
  });
  if (!response.ok) throw new Error(`repository creation failed: ${await response.text()}`);
  return (await response.json()) as {
    name: string;
    repositoryId: string;
    incarnation: string;
  };
}
