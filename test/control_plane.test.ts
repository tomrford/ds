import { env, exports } from "cloudflare:workers";
import { evictDurableObject } from "cloudflare:test";
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
    });
    expect(
      await control.authorizeRepository(stranger, created.repositoryId, created.incarnation),
    ).toEqual({ ok: false, status: 404, error: "repository not found" });
    const repository = env.REPOSITORIES.get(env.REPOSITORIES.idFromString(created.repositoryId));
    expect(
      await repository.putPackManifest(
        { ...stranger, repositoryId: created.repositoryId, incarnation: created.incarnation },
        packId,
        new Uint8Array(),
      ),
    ).toMatchObject({ ok: false, status: 409, error: "repository authority is stale" });
  });

  it("denies missing and invalid secrets and rejects malformed machine IDs", async () => {
    const missing = await exports.default.fetch(
      new Request("https://example.com/repositories/test", {
        headers: { "x-devspace-machine-id": "33".repeat(16) },
      }),
    );
    expect(missing.status).toBe(401);
    expect(await missing.json()).toEqual({ error: "unauthorized" });

    const invalid = await exports.default.fetch(
      new Request("https://example.com/repositories/test", {
        headers: {
          authorization: "Bearer invalid",
          "x-devspace-machine-id": "33".repeat(16),
        },
      }),
    );
    expect(invalid.status).toBe(401);
    expect(await invalid.json()).toEqual({ error: "unauthorized" });

    const malformedMachine = await apiRequest("not-a-machine", "/repositories/test");
    expect(malformedMachine.status).toBe(400);
    expect(await malformedMachine.json()).toEqual({ error: "invalid machine ID" });
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
    ).toEqual({ ok: false, status: 404, error: "repository not found" });

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
    expect(await injectedBody.json()).toEqual({
      error: "request fields must be exactly name, idempotencyKey",
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
    });
  });

  it("recovers a persisted retirement before recreating its name", async () => {
    const userId = env.DEVSPACE_DEVELOPMENT_USER_ID;
    const machineId = "42".repeat(16);
    const idempotencyKey = "42".repeat(16);
    const repository = await createRepository(machineId, "retiring-replay", idempotencyKey);
    const control = env.CONTROL_PLANE.getByName("directory");
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
    });
    expect(await (await apiRequest(machineId, "/repositories/retiring-replay")).json()).toEqual(
      replacement,
    );

    const repositoryStub = env.REPOSITORIES.get(
      env.REPOSITORIES.idFromString(repository.repositoryId),
    );
    expect(
      await repositoryStub.initializeRepository({
        userId,
        machineId,
        repositoryId: repository.repositoryId,
        incarnation: repository.incarnation,
      }),
    ).toMatchObject({ ok: false, status: 409 });
  });

  it("fences initialization when retirement reaches a repository first", async () => {
    const repositoryId = env.REPOSITORIES.newUniqueId().toString();
    const authority = {
      userId: "retire-before-init-user",
      machineId: "43".repeat(16),
      repositoryId,
      incarnation: "43".repeat(16),
    };
    const repository = env.REPOSITORIES.get(env.REPOSITORIES.idFromString(repositoryId));
    expect(await repository.retireRepository(authority)).toEqual({ ok: true, retired: true });
    expect(await repository.initializeRepository(authority)).toMatchObject({
      ok: false,
      status: 409,
      error: "repository authority does not match",
    });
  });

  it("retires a deleted incarnation before recreating the name", async () => {
    const machineId = "51".repeat(16);
    const first = await createRepository(machineId, "replaceable", "51".repeat(16));
    const firstStub = env.REPOSITORIES.get(env.REPOSITORIES.idFromString(first.repositoryId));
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
    expect(
      await firstStub.putPackManifest(
        {
          userId: env.DEVSPACE_DEVELOPMENT_USER_ID,
          machineId,
          repositoryId: first.repositoryId,
          incarnation: first.incarnation,
        },
        packId,
        new Uint8Array(),
      ),
    ).toMatchObject({ ok: false, status: 409, error: "repository authority is stale" });

    const second = await createRepository(machineId, "replaceable", "52".repeat(16));
    expect(second.repositoryId).not.toBe(first.repositoryId);
    expect(second.incarnation).not.toBe(first.incarnation);

    const staleHeaders = { "x-devspace-incarnation": first.incarnation };
    expect(
      (
        await apiRequest(
          machineId,
          `/repositories/${first.repositoryId}/heads?incarnation=${first.incarnation}`,
          { headers: staleHeaders },
        )
      ).status,
    ).toBe(404);
    expect(
      (
        await apiRequest(machineId, `/repositories/${first.repositoryId}/heads`, {
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
          `/repositories/${second.repositoryId}/heads?incarnation=${first.incarnation}`,
          { headers: staleHeaders },
        )
      ).status,
    ).toBe(404);

    const current = await apiRequest(
      machineId,
      `/repositories/${second.repositoryId}/heads?incarnation=${second.incarnation}`,
      { headers: { "x-devspace-incarnation": second.incarnation } },
    );
    expect(current.status).toBe(200);
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
