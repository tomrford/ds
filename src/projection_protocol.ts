import { z } from "zod";

import { compareBytes, toHex } from "./kernel";
import {
  firstZodMessage,
  lowerHexBytesSchema,
  nonNegativeSafeIntegerSchema,
} from "./validation";

export const MAX_PROJECTION_REQUEST_BYTES = 4 * 1024 * 1024;
export const MAX_PROJECTION_REFS = 256;
export const MAX_PROJECTION_STATES = 8_192;
export const MAX_FETCH_RECEIPTS = MAX_PROJECTION_STATES;
export const MAX_REPOSITORY_PROJECTION_REFS = 512;
export const MAX_PROJECTION_NAME_BYTES = 256;

const encoder = new TextEncoder();

const shortIdSchema = (label: string) => lowerHexBytesSchema(16, label);
const nonZeroHexSchema = (bytes: number, label: string) =>
  lowerHexBytesSchema(bytes, label).refine((value) => value.some((byte) => byte !== 0), {
    error: `${label} must not be zero`,
  });
const gitOidSchema = (label: string) => nonZeroHexSchema(20, label);
const objectIdSchema = (label: string) => nonZeroHexSchema(64, label);
const nullableGitOidSchema = (label: string) => gitOidSchema(label).nullable();
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
    .refine((value) => encoder.encode(value).byteLength <= MAX_PROJECTION_NAME_BYTES, {
      error: `${label} exceeds ${MAX_PROJECTION_NAME_BYTES} UTF-8 bytes`,
    });

const projectionStateSchema = z.strictObject({
  gitOid: gitOidSchema("gitOid"),
  canonicalCommitId: objectIdSchema("canonicalCommitId"),
  publicCommitId: objectIdSchema("publicCommitId"),
  hiddenSetId: lowerHexBytesSchema(64, "hiddenSetId").nullable(),
});

const projectionUpdateSchema = z.strictObject({
  bookmark: nameSchema("bookmark"),
  expectedOldOid: nullableGitOidSchema("expectedOldOid"),
  states: z.array(projectionStateSchema),
  proposedState: nonNegativeSafeIntegerSchema.nullable(),
});

const beginProjectionBatchSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  batchId: shortIdSchema("batchId"),
  machineId: shortIdSchema("machineId"),
  remote: nameSchema("remote"),
  updates: z.array(projectionUpdateSchema).min(1).max(MAX_PROJECTION_REFS),
});

const fetchReceiptSchema = z.strictObject({
  gitOid: gitOidSchema("gitOid"),
  publicCommitId: objectIdSchema("publicCommitId"),
});

const fetchRefSchema = z.strictObject({
  bookmark: nameSchema("bookmark"),
  observedGitOid: gitOidSchema("observedGitOid"),
  expectedCursorOid: nullableGitOidSchema("expectedCursorOid"),
  states: z.array(projectionStateSchema),
  proposedState: nonNegativeSafeIntegerSchema.nullable(),
});

const recordFetchSchema = z
  .strictObject({
    incarnation: shortIdSchema("incarnation"),
    fetchId: shortIdSchema("fetchId"),
    machineId: shortIdSchema("machineId"),
    remote: nameSchema("remote"),
    refs: z.array(fetchRefSchema).max(MAX_PROJECTION_REFS),
    receipts: z.array(fetchReceiptSchema).max(MAX_FETCH_RECEIPTS),
  })
  .refine((request) => request.refs.length !== 0 || request.receipts.length !== 0, {
    error: "fetch request must include refs or receipts",
  });

const claimProjectionBatchSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  machineId: shortIdSchema("machineId"),
});

const projectionFenceSchema = z.strictObject({
  incarnation: shortIdSchema("incarnation"),
  machineId: shortIdSchema("machineId"),
  fence: nonNegativeSafeIntegerSchema,
});

const projectionObservationSchema = z.strictObject({
  bookmark: nameSchema("bookmark"),
  liveOid: nullableGitOidSchema("liveOid"),
});

const recoverProjectionBatchSchema = projectionFenceSchema.extend({
  observations: z.array(projectionObservationSchema).max(MAX_PROJECTION_REFS),
});

export type ProjectionState = z.output<typeof projectionStateSchema>;
export type ProjectionUpdate = z.output<typeof projectionUpdateSchema>;
export type BeginProjectionBatchRequest = z.output<typeof beginProjectionBatchSchema>;
export type FetchReceipt = z.output<typeof fetchReceiptSchema>;
export type FetchRef = z.output<typeof fetchRefSchema>;
export type RecordFetchRequest = z.output<typeof recordFetchSchema>;
export type ProjectionFenceRequest = z.output<typeof projectionFenceSchema>;
export type ClaimProjectionBatchRequest = z.output<typeof claimProjectionBatchSchema>;
export type ProjectionObservation = z.output<typeof projectionObservationSchema>;
export type RecoverProjectionBatchRequest = z.output<typeof recoverProjectionBatchSchema>;

export class ProjectionProtocolError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
  }
}

export function decodeBeginProjectionBatch(value: unknown): BeginProjectionBatchRequest {
  const request = parseProjection(beginProjectionBatchSchema, value);
  for (const [index, update] of request.updates.entries()) validateUpdate(update, index);
  requireStateLimit(request.updates, "updates");
  request.updates.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(request.updates, "updates");
  return request;
}

export function decodeRecordFetch(value: unknown): RecordFetchRequest {
  const request = parseProjection(recordFetchSchema, value);
  for (const [index, ref] of request.refs.entries()) {
    if (ref.proposedState !== null && ref.proposedState >= ref.states.length) {
      throw new Error(`refs[${index}].proposedState is outside states`);
    }
    if (
      ref.proposedState !== null &&
      compareBytes(ref.states[ref.proposedState].gitOid, ref.observedGitOid) !== 0
    ) {
      throw new Error(`refs[${index}].proposedState must map the observed Git ID`);
    }
  }
  requireStateLimit(request.refs, "refs");
  request.refs.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(request.refs, "refs");
  request.receipts.sort((left, right) => compareBytes(left.gitOid, right.gitOid));
  return request;
}

export function decodeClaimProjectionBatch(value: unknown): ClaimProjectionBatchRequest {
  return parseProjection(claimProjectionBatchSchema, value);
}

export function decodeRecoverProjectionBatch(value: unknown): RecoverProjectionBatchRequest {
  const request = parseProjection(recoverProjectionBatchSchema, value);
  request.observations.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(request.observations, "observations");
  return request;
}

export function decodeProjectionShortId(value: unknown, label: string): Uint8Array {
  return shortIdSchema(label).parse(value);
}

export function canonicalProjectionBatchBytes(request: BeginProjectionBatchRequest): Uint8Array {
  return encoder.encode(
    JSON.stringify({
      incarnation: toHex(request.incarnation),
      batchId: toHex(request.batchId),
      machineId: toHex(request.machineId),
      remote: request.remote,
      updates: request.updates.map((update) => ({
        bookmark: update.bookmark,
        expectedOldOid: update.expectedOldOid === null ? null : toHex(update.expectedOldOid),
        states: update.states.map(encodeProjectionState),
        proposedState: update.proposedState,
      })),
    }),
  );
}

export function canonicalFetchBytes(request: RecordFetchRequest): Uint8Array {
  return encoder.encode(
    JSON.stringify({
      incarnation: toHex(request.incarnation),
      fetchId: toHex(request.fetchId),
      machineId: toHex(request.machineId),
      remote: request.remote,
      refs: request.refs.map((ref) => ({
        bookmark: ref.bookmark,
        observedGitOid: toHex(ref.observedGitOid),
        expectedCursorOid:
          ref.expectedCursorOid === null ? null : toHex(ref.expectedCursorOid),
        states: ref.states.map(encodeProjectionState),
        proposedState: ref.proposedState,
      })),
      receipts: request.receipts.map((receipt) => ({
        gitOid: toHex(receipt.gitOid),
        publicCommitId: toHex(receipt.publicCommitId),
      })),
    }),
  );
}

export function compareNullableBytes(
  left: Uint8Array | null,
  right: Uint8Array | null,
): number {
  if (left === null) return right === null ? 0 : -1;
  if (right === null) return 1;
  return compareBytes(left, right);
}

function parseProjection<T>(schema: z.ZodType<T>, value: unknown): T {
  const result = schema.safeParse(value);
  if (result.success) return result.data;
  if (
    result.error.issues.some(
      (issue) => issue.code === "custom" && issue.path.length === 0,
    )
  ) {
    throw new ProjectionProtocolError(
      "fetch request must include refs or receipts",
      "fetch-empty",
    );
  }
  if (result.error.issues.some((issue) => issue.path.includes("hiddenSetId"))) {
    throw new ProjectionProtocolError(
      "hiddenSetId must be null or 128 lowercase hex characters",
      "invalid-hidden-set-id",
    );
  }
  throw new Error(firstZodMessage(result.error));
}

function validateUpdate(update: ProjectionUpdate, index: number) {
  if (update.proposedState !== null && update.proposedState >= update.states.length) {
    throw new Error(`updates[${index}].proposedState is outside states`);
  }
  if (update.proposedState === null && update.states.length !== 0) {
    throw new Error(`updates[${index}].states must be empty for a deletion`);
  }
  const stateKeys = update.states
    .map((state) => `${toHex(state.gitOid)}:${toHex(state.canonicalCommitId)}`)
    .sort();
  if (stateKeys.some((key, stateIndex) => stateIndex > 0 && key === stateKeys[stateIndex - 1])) {
    throw new Error(`updates[${index}].states must not contain duplicate mappings`);
  }
}

function requireStateLimit(values: Array<{ states: ProjectionState[] }>, field: string) {
  const count = values.reduce((total, value) => total + value.states.length, 0);
  if (count > MAX_PROJECTION_STATES) {
    throw new Error(`${field} exceeds the ${MAX_PROJECTION_STATES}-state limit`);
  }
}

function encodeProjectionState(state: ProjectionState) {
  return {
    gitOid: toHex(state.gitOid),
    canonicalCommitId: toHex(state.canonicalCommitId),
    publicCommitId: toHex(state.publicCommitId),
    hiddenSetId: state.hiddenSetId === null ? null : toHex(state.hiddenSetId),
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
  return compareBytes(encoder.encode(left), encoder.encode(right));
}
