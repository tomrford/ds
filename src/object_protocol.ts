import { compareBytes, fromHex } from "./kernel";
import { decodeIncarnation } from "./head_protocol";

export const MAX_OBJECT_INVENTORY_KEYS = 4_096;
export const MAX_OBJECT_INVENTORY_REQUEST_BYTES = 640 * 1_024;

export interface InventoryObject {
  kind: number;
  id: Uint8Array;
}

export interface ObjectInventoryRequest {
  incarnation: Uint8Array;
  objects: InventoryObject[];
}

export function decodeObjectInventory(value: unknown): ObjectInventoryRequest {
  if (!isRecord(value)) throw new Error("object inventory request must be a JSON object");
  requireExactKeys(value, ["incarnation", "objects"]);
  const incarnation = decodeIncarnation(value.incarnation);
  if (!Array.isArray(value.objects)) throw new Error("objects must be an array");
  if (value.objects.length > MAX_OBJECT_INVENTORY_KEYS) {
    throw new Error(`objects exceeds the ${MAX_OBJECT_INVENTORY_KEYS}-object limit`);
  }
  const objects = value.objects.map((object, index) => decodeObject(object, index));
  for (let index = 1; index < objects.length; index += 1) {
    if (compareObjects(objects[index - 1], objects[index]) >= 0) {
      throw new Error("objects must be strictly sorted and unique");
    }
  }
  return { incarnation, objects };
}

function decodeObject(value: unknown, index: number): InventoryObject {
  if (!isRecord(value)) throw new Error(`objects[${index}] must be a JSON object`);
  requireExactKeys(value, ["kind", "id"]);
  if (!Number.isInteger(value.kind) || (value.kind as number) < 0 || (value.kind as number) > 5) {
    throw new Error(`objects[${index}].kind must be an integer from 0 through 5`);
  }
  if (typeof value.id !== "string") {
    throw new Error(`objects[${index}].id must be a string`);
  }
  let id: Uint8Array;
  try {
    id = fromHex(value.id);
  } catch {
    throw new Error(`objects[${index}].id must be 128 lowercase hex characters`);
  }
  return { kind: value.kind as number, id };
}

function compareObjects(left: InventoryObject, right: InventoryObject): number {
  return left.kind - right.kind || compareBytes(left.id, right.id);
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
    throw new Error(`object inventory request fields must be exactly ${expected.join(", ")}`);
  }
}
