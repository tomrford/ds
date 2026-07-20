import { z } from "zod";

import { compareBytes } from "./kernel";
import { lowerHexBytesSchema } from "./validation";

export const MAX_OBJECT_INVENTORY_KEYS = 4_096;
export const MAX_OBJECT_INVENTORY_REQUEST_BYTES = 640 * 1_024;

const inventoryObjectSchema = z.strictObject({
  kind: z.number().int().min(0).max(5),
  id: lowerHexBytesSchema(64, "object ID"),
});

const objectInventorySchema = z
  .strictObject({
    incarnation: lowerHexBytesSchema(16, "incarnation"),
    objects: z.array(inventoryObjectSchema).max(MAX_OBJECT_INVENTORY_KEYS),
  })
  .superRefine((request, context) => {
    for (let index = 1; index < request.objects.length; index += 1) {
      if (compareObjects(request.objects[index - 1], request.objects[index]) >= 0) {
        context.addIssue({
          code: "custom",
          path: ["objects", index],
          message: "objects must be strictly sorted and unique",
        });
      }
    }
  });

export type InventoryObject = z.output<typeof inventoryObjectSchema>;
export type ObjectInventoryRequest = z.output<typeof objectInventorySchema>;

export function decodeObjectInventory(value: unknown): ObjectInventoryRequest {
  return objectInventorySchema.parse(value);
}

function compareObjects(left: InventoryObject, right: InventoryObject): number {
  return left.kind - right.kind || compareBytes(left.id, right.id);
}
