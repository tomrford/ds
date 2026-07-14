import { equalBytes } from "./kernel";

const MANIFEST_MAGIC = new Uint8Array([0x44, 0x53, 0x50, 0x4b]);
const MANIFEST_VERSION = 1;
const MANIFEST_HEADER_BYTES = 96;
const OBJECT_ENTRY_BYTES = 88;
const CHUNK_ENTRY_BYTES = 80;

export const MIN_CHUNK_BYTES = 64 * 1024;
export const MAX_CHUNK_BYTES = 8 * 1024 * 1024;
export const MAX_PACK_BYTES = 64 * 1024 * 1024;
export const MAX_OBJECT_BYTES = 1024 * 1024;
export const MAX_PACK_OBJECTS = 65_536;
export const MAX_PACK_HEADS = 4_096;
export const MAX_PACK_CHUNKS = MAX_PACK_BYTES / MIN_CHUNK_BYTES;
export const MAX_MANIFEST_BYTES =
  MANIFEST_HEADER_BYTES +
  MAX_PACK_HEADS * 64 +
  MAX_PACK_OBJECTS * OBJECT_ENTRY_BYTES +
  MAX_PACK_CHUNKS * CHUNK_ENTRY_BYTES;
export const STORAGE_PART_BYTES = 1024 * 1024;

export interface ManifestObject {
  kind: number;
  id: Uint8Array;
  offset: number;
  length: number;
}

export interface ManifestChunk {
  offset: number;
  length: number;
  hash: Uint8Array;
}

export interface PackManifest {
  chunkBytes: number;
  packLength: number;
  packHash: Uint8Array;
  operationHeads: Uint8Array[];
  objects: ManifestObject[];
  chunks: ManifestChunk[];
}

export function decodeManifest(bytes: Uint8Array): PackManifest {
  if (bytes.byteLength < MANIFEST_HEADER_BYTES) throw new Error("manifest header is truncated");
  if (!equalBytes(bytes.subarray(0, 4), MANIFEST_MAGIC)) throw new Error("invalid manifest magic");
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (view.getUint16(4, true) !== MANIFEST_VERSION) throw new Error("unsupported manifest version");
  requireZero(bytes.subarray(6, 8), "manifest header reserved bytes");
  const chunkBytes = view.getUint32(8, true);
  const headCount = view.getUint32(12, true);
  const objectCount = view.getUint32(16, true);
  const chunkCount = view.getUint32(20, true);
  const packLength = readLength(view, 24, "pack length");
  if (chunkBytes < MIN_CHUNK_BYTES || chunkBytes > MAX_CHUNK_BYTES) {
    throw new Error("manifest chunk size is outside the canonical range");
  }
  if (headCount > MAX_PACK_HEADS) throw new Error("manifest has too many operation heads");
  if (objectCount > MAX_PACK_OBJECTS) throw new Error("manifest has too many objects");
  if (chunkCount > MAX_PACK_CHUNKS) throw new Error("manifest has too many chunks");
  if (packLength > MAX_PACK_BYTES) throw new Error("manifest pack exceeds the byte limit");
  const expectedLength =
    MANIFEST_HEADER_BYTES +
    headCount * 64 +
    objectCount * OBJECT_ENTRY_BYTES +
    chunkCount * CHUNK_ENTRY_BYTES;
  if (bytes.byteLength !== expectedLength) throw new Error("manifest length does not match counts");

  const packHash = bytes.slice(32, 96);
  let offset = MANIFEST_HEADER_BYTES;
  const operationHeads: Uint8Array[] = [];
  for (let index = 0; index < headCount; index += 1) {
    operationHeads.push(bytes.slice(offset, offset + 64));
    offset += 64;
  }
  requireStrictOrder(operationHeads, compareBytes, "operation heads");

  const objects: ManifestObject[] = [];
  for (let index = 0; index < objectCount; index += 1) {
    const kind = bytes[offset];
    if (kind > 5) throw new Error(`manifest object ${index} has an unknown kind`);
    requireZero(bytes.subarray(offset + 1, offset + 8), `manifest object ${index} reserved bytes`);
    objects.push({
      kind,
      id: bytes.slice(offset + 8, offset + 72),
      offset: readLength(view, offset + 72, `object ${index} offset`),
      length: readLength(view, offset + 80, `object ${index} length`),
    });
    offset += OBJECT_ENTRY_BYTES;
  }
  requireStrictOrder(objects, compareObjects, "objects");
  validateRanges(
    objects.map((object) => [object.offset, object.length]),
    packLength,
    undefined,
    "object",
  );
  if (objects.some((object) => object.length > MAX_OBJECT_BYTES)) {
    throw new Error("manifest object exceeds the byte limit");
  }

  const chunks: ManifestChunk[] = [];
  for (let index = 0; index < chunkCount; index += 1) {
    const chunkOffset = readLength(view, offset, `chunk ${index} offset`);
    const length = view.getUint32(offset + 8, true);
    requireZero(bytes.subarray(offset + 12, offset + 16), `manifest chunk ${index} reserved bytes`);
    chunks.push({
      offset: chunkOffset,
      length,
      hash: bytes.slice(offset + 16, offset + 80),
    });
    offset += CHUNK_ENTRY_BYTES;
  }
  validateRanges(
    chunks.map((chunk) => [chunk.offset, chunk.length]),
    packLength,
    chunkBytes,
    "chunk",
  );
  return { chunkBytes, packLength, packHash, operationHeads, objects, chunks };
}

function readLength(view: DataView, offset: number, field: string): number {
  const value = view.getBigUint64(offset, true);
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error(`${field} is too large`);
  return Number(value);
}

function requireZero(bytes: Uint8Array, field: string) {
  if (bytes.some((byte) => byte !== 0)) throw new Error(`${field} must be zero`);
}

function requireStrictOrder<T>(
  values: T[],
  compare: (left: T, right: T) => number,
  field: string,
) {
  for (let index = 1; index < values.length; index += 1) {
    if (compare(values[index - 1], values[index]) >= 0) {
      throw new Error(`manifest ${field} are not strictly sorted and unique`);
    }
  }
}

function compareObjects(left: ManifestObject, right: ManifestObject): number {
  return left.kind - right.kind || compareBytes(left.id, right.id);
}

function compareBytes(left: Uint8Array, right: Uint8Array): number {
  for (let index = 0; index < left.byteLength; index += 1) {
    const difference = left[index] - right[index];
    if (difference !== 0) return difference;
  }
  return left.byteLength - right.byteLength;
}

function validateRanges(
  ranges: Array<[number, number]>,
  packLength: number,
  chunkBytes: number | undefined,
  field: string,
) {
  let expectedOffset = 0;
  for (const [index, [offset, length]] of ranges.entries()) {
    const invalidChunk =
      chunkBytes !== undefined &&
      (length === 0 || length > chunkBytes || (index + 1 < ranges.length && length !== chunkBytes));
    if (offset !== expectedOffset || invalidChunk) {
      throw new Error(`manifest ${field} range ${index} is not canonical`);
    }
    expectedOffset += length;
    if (!Number.isSafeInteger(expectedOffset)) throw new Error(`manifest ${field} ranges overflow`);
  }
  if (expectedOffset !== packLength) throw new Error(`manifest ${field} ranges do not fill pack`);
}

export function splitParts(bytes: Uint8Array): Uint8Array[] {
  const parts: Uint8Array[] = [];
  for (let offset = 0; offset < bytes.byteLength; offset += STORAGE_PART_BYTES) {
    parts.push(bytes.slice(offset, offset + STORAGE_PART_BYTES));
  }
  return parts;
}

export function concatParts(parts: Uint8Array[], limit: number): Uint8Array {
  const length = parts.reduce((total, part) => total + part.byteLength, 0);
  if (length > limit) throw new Error(`stored bytes exceed ${limit}-byte limit`);
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const part of parts) {
    bytes.set(part, offset);
    offset += part.byteLength;
  }
  return bytes;
}

export async function readBoundedBody(
  request: Request,
  limit: number,
  object: string,
): Promise<Uint8Array> {
  const declaredLength = request.headers.get("content-length");
  if (declaredLength !== null && Number(declaredLength) > limit) {
    throw new Error(`${object} exceeds ${limit} byte limit`);
  }
  if (request.body === null) return new Uint8Array();
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let length = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    length += value.byteLength;
    if (length > limit) {
      await reader.cancel();
      throw new Error(`${object} exceeds ${limit} byte limit`);
    }
    chunks.push(value);
  }
  return concatParts(chunks, limit);
}
