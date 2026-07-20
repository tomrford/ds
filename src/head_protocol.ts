import { z } from "zod";

import { compareBytes } from "./kernel";
import { lowerHexBytesSchema } from "./validation";

export const MAX_OPERATION_HEADS = 4_096;
export const MAX_OBSERVED_HEADS = MAX_OPERATION_HEADS;
export const MAX_HEAD_REQUEST_BYTES = 640 * 1_024;

const shortHex = (label: string) => lowerHexBytesSchema(16, label);
const operationId = (label: string) => lowerHexBytesSchema(64, label);

const headTransactionSchema = z
  .strictObject({
    incarnation: shortHex("incarnation"),
    idempotencyKey: shortHex("idempotencyKey"),
    newHead: operationId("newHead").refine((value) => value.some((byte) => byte !== 0), {
      error: "newHead must not be the implicit zero operation",
    }),
    observedHeads: z.array(operationId("observed head")).max(MAX_OBSERVED_HEADS),
  })
  .transform((request, context) => {
    request.observedHeads.sort(compareBytes);
    for (let index = 1; index < request.observedHeads.length; index += 1) {
      if (compareBytes(request.observedHeads[index - 1], request.observedHeads[index]) === 0) {
        context.addIssue({
          code: "custom",
          path: ["observedHeads", index],
          message: "observedHeads must not contain duplicates",
        });
      }
    }
    return request;
  });

export type HeadTransactionRequest = z.output<typeof headTransactionSchema>;

export function decodeIncarnation(value: unknown): Uint8Array {
  return shortHex("incarnation").parse(value);
}

export function decodeHeadTransaction(value: unknown): HeadTransactionRequest {
  return headTransactionSchema.parse(value);
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
