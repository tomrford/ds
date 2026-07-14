import { fromHex } from "./kernel";

export const MAX_OPERATION_HEADS = 4_096;
export const MAX_OBSERVED_HEADS = MAX_OPERATION_HEADS;
export const MAX_HEAD_REQUEST_BYTES = 640 * 1_024;

const INCARNATION_PATTERN = /^[0-9a-f]{32}$/;

export interface HeadTransactionRequest {
  incarnation: Uint8Array;
  idempotencyKey: Uint8Array;
  newHead: Uint8Array;
  observedHeads: Uint8Array[];
}

export function decodeIncarnation(value: unknown): Uint8Array {
  if (typeof value !== "string" || !INCARNATION_PATTERN.test(value)) {
    throw new Error("incarnation must be 32 lowercase hex characters");
  }
  return decodeShortHex(value);
}

export function decodeHeadTransaction(value: unknown): HeadTransactionRequest {
  if (!isRecord(value)) throw new Error("head request must be a JSON object");
  requireExactKeys(value, ["incarnation", "idempotencyKey", "newHead", "observedHeads"]);
  const incarnation = decodeIncarnation(value.incarnation);
  const idempotencyKey = decodeIdempotencyKey(value.idempotencyKey);
  const newHead = decodeHead(value.newHead, "newHead");
  if (!Array.isArray(value.observedHeads)) throw new Error("observedHeads must be an array");
  if (value.observedHeads.length > MAX_OBSERVED_HEADS) {
    throw new Error(`observedHeads exceeds the ${MAX_OBSERVED_HEADS}-head limit`);
  }
  const observedHeads = value.observedHeads.map((head, index) =>
    decodeHead(head, `observedHeads[${index}]`),
  );
  observedHeads.sort(compareBytes);
  for (let index = 1; index < observedHeads.length; index += 1) {
    if (compareBytes(observedHeads[index - 1], observedHeads[index]) === 0) {
      throw new Error("observedHeads must not contain duplicates");
    }
  }
  return { incarnation, idempotencyKey, newHead, observedHeads };
}

export function canonicalHeadTransactionBytes(request: HeadTransactionRequest): Uint8Array {
  const bytes = new Uint8Array(8 + 16 + 64 + request.observedHeads.length * 64);
  const view = new DataView(bytes.buffer);
  bytes.set(new TextEncoder().encode("DSHD"));
  view.setUint16(4, 1, true);
  view.setUint16(6, request.observedHeads.length, true);
  bytes.set(request.incarnation, 8);
  bytes.set(request.newHead, 24);
  for (const [index, head] of request.observedHeads.entries()) {
    bytes.set(head, 88 + index * 64);
  }
  return bytes;
}

function decodeIdempotencyKey(value: unknown): Uint8Array {
  if (typeof value !== "string" || !INCARNATION_PATTERN.test(value)) {
    throw new Error("idempotencyKey must be 32 lowercase hex characters");
  }
  return decodeShortHex(value);
}

function decodeHead(value: unknown, field: string): Uint8Array {
  if (typeof value !== "string") throw new Error(`${field} must be a string`);
  try {
    return fromHex(value);
  } catch {
    throw new Error(`${field} must be 128 lowercase hex characters`);
  }
}

function decodeShortHex(value: string): Uint8Array {
  return Uint8Array.from({ length: 16 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

function compareBytes(left: Uint8Array, right: Uint8Array): number {
  for (let index = 0; index < left.byteLength; index += 1) {
    const difference = left[index] - right[index];
    if (difference !== 0) return difference;
  }
  return left.byteLength - right.byteLength;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function requireExactKeys(value: Record<string, unknown>, expected: string[]) {
  const keys = Object.keys(value).sort();
  const sortedExpected = [...expected].sort();
  if (
    keys.length !== sortedExpected.length ||
    keys.some((key, index) => key !== sortedExpected[index])
  ) {
    throw new Error(`head request fields must be exactly ${expected.join(", ")}`);
  }
}
