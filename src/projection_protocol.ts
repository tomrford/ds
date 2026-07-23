/**
 * Projection journal wire contract.
 *
 * A state is a canonical/public Git OID pair plus its nullable hidden-set
 * identity. Request bytes, refs, states, repository refs, and name bounds are
 * 4 MiB, 256, 8,192, 512, and 256 UTF-8 bytes respectively.
 */
import { z } from "zod";

import { compareGitBytes, gitToHex } from "./kernel";
import {
  firstZodMessage,
  lowerHexBytesSchema,
  nonNegativeSafeIntegerSchema,
} from "./validation";

export const MAX_GIT_PROJECTION_REQUEST_BYTES = 4 * 1024 * 1024;
export const MAX_GIT_PROJECTION_REFS = 256;
export const MAX_GIT_PROJECTION_STATES = 8_192;
export const MAX_REPOSITORY_GIT_PROJECTION_REFS = 512;
export const MAX_GIT_PROJECTION_NAME_BYTES = 256;

const encoder = new TextEncoder();

const shortIdSchema = (label: string) => lowerHexBytesSchema(16, label);
const nonZeroOidSchema = (label: string) =>
  lowerHexBytesSchema(20, label).refine((value) => value.some((byte) => byte !== 0), {
    error: `${label} must not be zero`,
  });
const nullableOidSchema = (label: string) => nonZeroOidSchema(label).nullable();
const nameSchema = (label: string) =>
  z
    .string()
    .min(1, `${label} must be non-empty`)
    .refine(
      (value) =>
        ![...value].some((character) => {
          const code = character.codePointAt(0) ?? 0;
          return code < 0x20 || code === 0x7f || (code >= 0xd800 && code <= 0xdfff);
        }),
      { error: `${label} must not contain control characters` },
    )
    .refine((value) => encoder.encode(value).byteLength <= MAX_GIT_PROJECTION_NAME_BYTES, {
      error: `${label} exceeds ${MAX_GIT_PROJECTION_NAME_BYTES} UTF-8 bytes`,
    });

const projectionGitStateSchema = z.strictObject({
  canonicalOid: nonZeroOidSchema("canonicalOid"),
  publicOid: nonZeroOidSchema("publicOid"),
  hiddenSetId: lowerHexBytesSchema(64, "hiddenSetId").nullable(),
});

const projectionGitUpdateSchema = z.strictObject({
  bookmark: nameSchema("bookmark"),
  expectedOldOid: nullableOidSchema("expectedOldOid"),
  states: z.array(projectionGitStateSchema),
  proposedState: nonNegativeSafeIntegerSchema.nullable(),
  identityOid: nullableOidSchema("identityOid"),
});

const beginProjectionGitBatchSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  batchId: shortIdSchema("batchId"),
  machineId: shortIdSchema("machineId"),
  remote: nameSchema("remote"),
  updates: z.array(projectionGitUpdateSchema).min(1).max(MAX_GIT_PROJECTION_REFS),
});

const fetchGitRefSchema = z.strictObject({
  bookmark: nameSchema("bookmark"),
  observedPublicOid: nonZeroOidSchema("observedPublicOid"),
  expectedCursorOid: nullableOidSchema("expectedCursorOid"),
  states: z.array(projectionGitStateSchema),
  proposedState: nonNegativeSafeIntegerSchema.nullable(),
  identityOid: nullableOidSchema("identityOid"),
});

const recordGitFetchSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  fetchId: shortIdSchema("fetchId"),
  machineId: shortIdSchema("machineId"),
  remote: nameSchema("remote"),
  refs: z.array(fetchGitRefSchema).min(1).max(MAX_GIT_PROJECTION_REFS),
});

const claimProjectionGitBatchSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  machineId: shortIdSchema("machineId"),
});

const projectionGitFenceSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  machineId: shortIdSchema("machineId"),
  fence: nonNegativeSafeIntegerSchema,
});

const projectionGitObservationSchema = z.strictObject({
  bookmark: nameSchema("bookmark"),
  liveOid: nullableOidSchema("liveOid"),
});

const recoverProjectionGitBatchSchema = projectionGitFenceSchema.extend({
  observations: z.array(projectionGitObservationSchema).max(MAX_GIT_PROJECTION_REFS),
});

export type ProjectionGitState = z.output<typeof projectionGitStateSchema>;
export type ProjectionGitUpdate = z.output<typeof projectionGitUpdateSchema>;
export type BeginProjectionGitBatchRequest = z.output<typeof beginProjectionGitBatchSchema>;
export type FetchGitRef = z.output<typeof fetchGitRefSchema>;
export type RecordGitFetchRequest = z.output<typeof recordGitFetchSchema>;
export type ProjectionGitFenceRequest = z.output<typeof projectionGitFenceSchema>;
export type ClaimProjectionGitBatchRequest = z.output<typeof claimProjectionGitBatchSchema>;
export type ProjectionGitObservation = z.output<typeof projectionGitObservationSchema>;
export type RecoverProjectionGitBatchRequest = z.output<typeof recoverProjectionGitBatchSchema>;

export class ProjectionGitProtocolError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
  }
}

export function decodeBeginProjectionGitBatch(value: unknown): BeginProjectionGitBatchRequest {
  const request = parseProjectionGit(beginProjectionGitBatchSchema, value);
  for (const [index, update] of request.updates.entries()) validateUpdate(update, index);
  requireStateLimit(request.updates, "updates");
  request.updates.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(request.updates, "updates");
  return request;
}

export function decodeRecordGitFetch(value: unknown): RecordGitFetchRequest {
  const request = parseProjectionGit(recordGitFetchSchema, value);
  for (const [index, ref] of request.refs.entries()) {
    if (ref.proposedState !== null && ref.proposedState >= ref.states.length) {
      throw new Error(`refs[${index}].proposedState is outside states`);
    }
    if (
      ref.proposedState !== null &&
      compareGitBytes(ref.states[ref.proposedState].publicOid, ref.observedPublicOid) !== 0
    ) {
      throw new Error(`refs[${index}].proposedState must map the observed public OID`);
    }
    if (ref.identityOid !== null) {
      if (ref.proposedState !== null || ref.states.length !== 0) {
        throw new Error(`refs[${index}].identityOid requires no states or proposedState`);
      }
      if (compareGitBytes(ref.identityOid, ref.observedPublicOid) !== 0) {
        throw new Error(`refs[${index}].identityOid must equal the observed public OID`);
      }
    }
  }
  requireStateLimit(request.refs, "refs");
  request.refs.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(request.refs, "refs");
  return request;
}

export function decodeClaimProjectionGitBatch(value: unknown): ClaimProjectionGitBatchRequest {
  return parseProjectionGit(claimProjectionGitBatchSchema, value);
}

export function decodeRecoverProjectionGitBatch(value: unknown): RecoverProjectionGitBatchRequest {
  const request = parseProjectionGit(recoverProjectionGitBatchSchema, value);
  request.observations.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(request.observations, "observations");
  return request;
}

export function decodeProjectionGitShortId(value: unknown, label: string): Uint8Array {
  return shortIdSchema(label).parse(value);
}

export function canonicalProjectionGitBatchBytes(
  request: BeginProjectionGitBatchRequest,
): Uint8Array {
  return encoder.encode(
    JSON.stringify({
      incarnation: gitToHex(request.incarnation),
      batchId: gitToHex(request.batchId),
      machineId: gitToHex(request.machineId),
      remote: request.remote,
      updates: request.updates.map((update) => ({
        bookmark: update.bookmark,
        expectedOldOid:
          update.expectedOldOid === null ? null : gitToHex(update.expectedOldOid),
        states: update.states.map(encodeProjectionGitState),
        proposedState: update.proposedState,
        identityOid: update.identityOid === null ? null : gitToHex(update.identityOid),
      })),
    }),
  );
}

export function canonicalGitFetchBytes(request: RecordGitFetchRequest): Uint8Array {
  return encoder.encode(
    JSON.stringify({
      incarnation: gitToHex(request.incarnation),
      fetchId: gitToHex(request.fetchId),
      machineId: gitToHex(request.machineId),
      remote: request.remote,
      refs: request.refs.map((ref) => ({
        bookmark: ref.bookmark,
        observedPublicOid: gitToHex(ref.observedPublicOid),
        expectedCursorOid:
          ref.expectedCursorOid === null ? null : gitToHex(ref.expectedCursorOid),
        states: ref.states.map(encodeProjectionGitState),
        proposedState: ref.proposedState,
        identityOid: ref.identityOid === null ? null : gitToHex(ref.identityOid),
      })),
    }),
  );
}

export function compareNullableGitOids(
  left: Uint8Array | null,
  right: Uint8Array | null,
): number {
  if (left === null) return right === null ? 0 : -1;
  if (right === null) return 1;
  return compareGitBytes(left, right);
}

function parseProjectionGit<T>(schema: z.ZodType<T>, value: unknown): T {
  const result = schema.safeParse(value);
  if (result.success) return result.data;
  if (result.error.issues.some((issue) => issue.path.includes("hiddenSetId"))) {
    throw new ProjectionGitProtocolError(
      "hiddenSetId must be null or 128 lowercase hex characters",
      "invalid-hidden-set-id",
    );
  }
  throw new Error(firstZodMessage(result.error));
}

function validateUpdate(update: ProjectionGitUpdate, index: number) {
  if (update.proposedState !== null && update.proposedState >= update.states.length) {
    throw new Error(`updates[${index}].proposedState is outside states`);
  }
  if (update.identityOid !== null) {
    if (update.proposedState !== null || update.states.length !== 0) {
      throw new Error(`updates[${index}].identityOid requires no states or proposedState`);
    }
  } else if (update.proposedState === null && update.states.length !== 0) {
    throw new Error(`updates[${index}].states must be empty without a proposed state`);
  }
  const stateKeys = update.states
    .map((state) => `${gitToHex(state.canonicalOid)}:${gitToHex(state.publicOid)}`)
    .sort();
  if (stateKeys.some((key, stateIndex) => stateIndex > 0 && key === stateKeys[stateIndex - 1])) {
    throw new Error(`updates[${index}].states must not contain duplicate mappings`);
  }
}

function requireStateLimit(values: Array<{ states: ProjectionGitState[] }>, field: string) {
  const count = values.reduce((total, value) => total + value.states.length, 0);
  if (count > MAX_GIT_PROJECTION_STATES) {
    throw new Error(`${field} exceeds the ${MAX_GIT_PROJECTION_STATES}-state limit`);
  }
}

function encodeProjectionGitState(state: ProjectionGitState) {
  return {
    canonicalOid: gitToHex(state.canonicalOid),
    publicOid: gitToHex(state.publicOid),
    hiddenSetId: state.hiddenSetId === null ? null : gitToHex(state.hiddenSetId),
  };
}

function requireUniqueNames(values: Array<{ bookmark: string }>, field: string) {
  for (let index = 1; index < values.length; index += 1) {
    if (values[index - 1].bookmark === values[index].bookmark) {
      throw new Error(`${field} must not contain duplicate bookmarks`);
    }
  }
}

function compareNames(left: string, right: string): number {
  return compareGitBytes(encoder.encode(left), encoder.encode(right));
}
