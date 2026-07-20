import { compareBytes, toHex } from "./kernel";

export const MAX_PROJECTION_REQUEST_BYTES = 4 * 1024 * 1024;
export const MAX_PROJECTION_REFS = 256;
export const MAX_PROJECTION_STATES = 8_192;
export const MAX_REPOSITORY_PROJECTION_REFS = 512;
export const MAX_PROJECTION_NAME_BYTES = 256;

const SHORT_ID_PATTERN = /^[0-9a-f]{32}$/;
const GIT_OID_PATTERN = /^[0-9a-f]{40}$/;
const OBJECT_ID_PATTERN = /^[0-9a-f]{128}$/;
const encoder = new TextEncoder();

export interface ProjectionState {
  gitOid: Uint8Array;
  canonicalCommitId: Uint8Array;
  publicCommitId: Uint8Array;
  hiddenSetId: Uint8Array | null;
}

export interface ProjectionUpdate {
  bookmark: string;
  expectedOldOid: Uint8Array | null;
  states: ProjectionState[];
  proposedState: number | null;
}

export interface BeginProjectionBatchRequest {
  incarnation: Uint8Array;
  batchId: Uint8Array;
  machineId: Uint8Array;
  remote: string;
  updates: ProjectionUpdate[];
}

export interface ProjectionFenceRequest {
  incarnation: Uint8Array;
  machineId: Uint8Array;
  fence: number;
}

export interface ClaimProjectionBatchRequest {
  incarnation: Uint8Array;
  machineId: Uint8Array;
}

export interface ProjectionObservation {
  bookmark: string;
  liveOid: Uint8Array | null;
}

export interface RecoverProjectionBatchRequest extends ProjectionFenceRequest {
  observations: ProjectionObservation[];
}

export class ProjectionProtocolError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
  }
}

export function decodeBeginProjectionBatch(value: unknown): BeginProjectionBatchRequest {
  const record = requireRecord(value, "projection batch");
  requireExactKeys(record, ["incarnation", "batchId", "machineId", "remote", "updates"]);
  const updatesValue = record.updates;
  if (!Array.isArray(updatesValue) || updatesValue.length === 0) {
    throw new Error("updates must be a non-empty array");
  }
  if (updatesValue.length > MAX_PROJECTION_REFS) {
    throw new Error(`updates exceeds the ${MAX_PROJECTION_REFS}-ref limit`);
  }
  const updates = updatesValue.map((update, index) => decodeUpdate(update, index));
  const stateCount = updates.reduce((count, update) => count + update.states.length, 0);
  if (stateCount > MAX_PROJECTION_STATES) {
    throw new Error(`updates exceeds the ${MAX_PROJECTION_STATES}-state limit`);
  }
  updates.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(updates, "updates");
  return {
    incarnation: decodeHex(record.incarnation, SHORT_ID_PATTERN, "incarnation", 16),
    batchId: decodeHex(record.batchId, SHORT_ID_PATTERN, "batchId", 16),
    machineId: decodeHex(record.machineId, SHORT_ID_PATTERN, "machineId", 16),
    remote: decodeName(record.remote, "remote"),
    updates,
  };
}

export function decodeClaimProjectionBatch(value: unknown): ClaimProjectionBatchRequest {
  const record = requireRecord(value, "projection claim");
  requireExactKeys(record, ["incarnation", "machineId"]);
  return {
    incarnation: decodeHex(record.incarnation, SHORT_ID_PATTERN, "incarnation", 16),
    machineId: decodeHex(record.machineId, SHORT_ID_PATTERN, "machineId", 16),
  };
}

export function decodeProjectionFence(value: unknown): ProjectionFenceRequest {
  const record = requireRecord(value, "projection fence");
  requireExactKeys(record, ["incarnation", "machineId", "fence"]);
  return {
    incarnation: decodeHex(record.incarnation, SHORT_ID_PATTERN, "incarnation", 16),
    machineId: decodeHex(record.machineId, SHORT_ID_PATTERN, "machineId", 16),
    fence: decodeSafeInteger(record.fence, "fence"),
  };
}

export function decodeRecoverProjectionBatch(value: unknown): RecoverProjectionBatchRequest {
  const record = requireRecord(value, "projection recovery");
  requireExactKeys(record, ["incarnation", "machineId", "fence", "observations"]);
  if (!Array.isArray(record.observations)) throw new Error("observations must be an array");
  if (record.observations.length > MAX_PROJECTION_REFS) {
    throw new Error(`observations exceeds the ${MAX_PROJECTION_REFS}-ref limit`);
  }
  const observations = record.observations.map((observation, index) => {
    const item = requireRecord(observation, `observations[${index}]`);
    requireExactKeys(item, ["bookmark", "liveOid"]);
    return {
      bookmark: decodeName(item.bookmark, `observations[${index}].bookmark`),
      liveOid: decodeNullableGitOid(item.liveOid, `observations[${index}].liveOid`),
    };
  });
  observations.sort((left, right) => compareNames(left.bookmark, right.bookmark));
  requireUniqueNames(observations, "observations");
  return {
    incarnation: decodeHex(record.incarnation, SHORT_ID_PATTERN, "incarnation", 16),
    machineId: decodeHex(record.machineId, SHORT_ID_PATTERN, "machineId", 16),
    fence: decodeSafeInteger(record.fence, "fence"),
    observations,
  };
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
        states: update.states.map((state) => ({
          gitOid: toHex(state.gitOid),
          canonicalCommitId: toHex(state.canonicalCommitId),
          publicCommitId: toHex(state.publicCommitId),
          hiddenSetId: state.hiddenSetId === null ? null : toHex(state.hiddenSetId),
        })),
        proposedState: update.proposedState,
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

function decodeUpdate(value: unknown, index: number): ProjectionUpdate {
  const record = requireRecord(value, `updates[${index}]`);
  requireExactKeys(record, ["bookmark", "expectedOldOid", "states", "proposedState"]);
  if (!Array.isArray(record.states)) throw new Error(`updates[${index}].states must be an array`);
  const states = record.states.map((value, stateIndex): ProjectionState => {
    const state = requireRecord(value, `updates[${index}].states[${stateIndex}]`);
    requireExactKeys(state, ["gitOid", "canonicalCommitId", "publicCommitId", "hiddenSetId"]);
    const decoded = {
      gitOid: decodeHex(state.gitOid, GIT_OID_PATTERN, "gitOid", 20),
      canonicalCommitId: decodeHex(
        state.canonicalCommitId,
        OBJECT_ID_PATTERN,
        "canonicalCommitId",
        64,
      ),
      publicCommitId: decodeHex(state.publicCommitId, OBJECT_ID_PATTERN, "publicCommitId", 64),
      hiddenSetId: decodeHiddenSetId(state.hiddenSetId),
    };
    requireNonZero(decoded.gitOid, "gitOid");
    requireNonZero(decoded.canonicalCommitId, "canonicalCommitId");
    requireNonZero(decoded.publicCommitId, "publicCommitId");
    return decoded;
  });
  const proposedState =
    record.proposedState === null
      ? null
      : decodeSafeInteger(record.proposedState, `updates[${index}].proposedState`);
  if (proposedState !== null && proposedState >= states.length) {
    throw new Error(`updates[${index}].proposedState is outside states`);
  }
  if (proposedState === null && states.length !== 0) {
    throw new Error(`updates[${index}].states must be empty for a deletion`);
  }
  const stateKeys = states
    .map((state) => `${toHex(state.gitOid)}:${toHex(state.canonicalCommitId)}`)
    .sort();
  if (stateKeys.some((key, stateIndex) => stateIndex > 0 && key === stateKeys[stateIndex - 1])) {
    throw new Error(`updates[${index}].states must not contain duplicate mappings`);
  }
  return {
    bookmark: decodeName(record.bookmark, `updates[${index}].bookmark`),
    expectedOldOid: decodeNullableGitOid(
      record.expectedOldOid,
      `updates[${index}].expectedOldOid`,
    ),
    states,
    proposedState,
  };
}

function decodeNullableGitOid(value: unknown, field: string): Uint8Array | null {
  if (value === null) return null;
  const oid = decodeHex(value, GIT_OID_PATTERN, field, 20);
  requireNonZero(oid, field);
  return oid;
}

function decodeHiddenSetId(value: unknown): Uint8Array | null {
  if (value === null) return null;
  if (typeof value !== "string" || !OBJECT_ID_PATTERN.test(value)) {
    throw new ProjectionProtocolError(
      "hiddenSetId must be null or 128 lowercase hex characters",
      "invalid-hidden-set-id",
    );
  }
  return decodeHex(value, OBJECT_ID_PATTERN, "hiddenSetId", 64);
}

function decodeHex(
  value: unknown,
  pattern: RegExp,
  field: string,
  expectedLength: number,
): Uint8Array {
  if (typeof value !== "string" || !pattern.test(value)) {
    throw new Error(`${field} has invalid lowercase hexadecimal encoding`);
  }
  const bytes = Uint8Array.from({ length: value.length / 2 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
  if (bytes.byteLength !== expectedLength) throw new Error(`${field} has invalid length`);
  return bytes;
}

function decodeName(value: unknown, field: string): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    [...value].some((character) => {
      const code = character.codePointAt(0) ?? 0;
      return code < 0x20 || code === 0x7f || (code >= 0xd800 && code <= 0xdfff);
    })
  ) {
    throw new Error(`${field} must be a non-empty string without control characters`);
  }
  if (encoder.encode(value).byteLength > MAX_PROJECTION_NAME_BYTES) {
    throw new Error(`${field} exceeds ${MAX_PROJECTION_NAME_BYTES} UTF-8 bytes`);
  }
  return value;
}

function requireNonZero(value: Uint8Array, field: string) {
  if (value.every((byte) => byte === 0)) throw new Error(`${field} must not be zero`);
}

function decodeSafeInteger(value: unknown, field: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${field} must be a non-negative safe integer`);
  }
  return value;
}

function requireRecord(value: unknown, field: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${field} must be a JSON object`);
  }
  return value as Record<string, unknown>;
}

function requireExactKeys(value: Record<string, unknown>, expected: string[]) {
  const keys = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (keys.length !== wanted.length || keys.some((key, index) => key !== wanted[index])) {
    throw new Error(`request fields must be exactly ${expected.join(", ")}`);
  }
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
