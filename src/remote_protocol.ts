import { z } from "zod";

import { MAX_PROJECTION_NAME_BYTES } from "./projection_protocol";
import {
  boundedStringSchema,
  firstZodMessage,
  lowerHexBytesSchema,
} from "./validation";

export const MAX_REMOTE_REQUEST_BYTES = 2 * 1024;
export const MAX_REMOTE_URL_BYTES = 1024;

const encoder = new TextEncoder();

const remoteNameSchema = boundedStringSchema("remote name", MAX_PROJECTION_NAME_BYTES).refine(
  validRemoteName,
  { error: "remote name must be a valid single-component Git ref name" },
);

const remoteUrlSchema = boundedStringSchema("remote URL", MAX_REMOTE_URL_BYTES)
  .refine((value) => !/[\r\n\u0085\u2028\u2029]/.test(value), {
    error: "remote URL must be a non-empty single-line string",
  })
  .refine((value) => !hasPasswordUserinfo(value), {
    error: "remote URL must not contain userinfo credentials",
  });

const setRemoteSchema = z.strictObject({
  incarnation: lowerHexBytesSchema(16, "incarnation"),
  url: remoteUrlSchema,
});

export type SetRemoteRequest = z.output<typeof setRemoteSchema>;

export class RemoteProtocolError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
  }
}

export function decodeRemoteName(value: unknown): string {
  const result = remoteNameSchema.safeParse(value);
  if (!result.success) {
    throw new RemoteProtocolError(firstZodMessage(result.error), "invalid-remote-name");
  }
  return result.data;
}

export function decodeSetRemote(value: unknown): SetRemoteRequest {
  const result = setRemoteSchema.safeParse(value);
  if (result.success) return result.data;
  const path = result.error.issues[0]?.path[0];
  let code = "invalid-remote-request";
  if (path === "incarnation") code = "invalid-incarnation";
  if (path === "url") {
    code = typeof (value as { url?: unknown })?.url === "string" && hasPasswordUserinfo((value as { url: string }).url)
      ? "credentials-in-remote-url"
      : "invalid-remote-url";
  }
  throw new RemoteProtocolError(firstZodMessage(result.error), code);
}

export function decodeRemoteIncarnation(value: unknown): Uint8Array {
  const result = lowerHexBytesSchema(16, "incarnation").safeParse(value);
  if (!result.success) {
    throw new RemoteProtocolError(firstZodMessage(result.error), "invalid-incarnation");
  }
  return result.data;
}

// Remote names appear as single Git ref components in the machine's
// observation refs, so registration enforces git-check-ref-format's
// single-component rules; the fetch transport shares this contract.
function validRemoteName(value: string): boolean {
  return !(
    value.startsWith("-") ||
    value.startsWith(".") ||
    value.endsWith(".") ||
    value.endsWith(".lock") ||
    value.includes("..") ||
    value.includes("@{") ||
    value === "@" ||
    [...value].some((character) => {
      const code = character.codePointAt(0) ?? 0;
      return (
        code <= 0x20 ||
        code === 0x7f ||
        (code >= 0xd800 && code <= 0xdfff) ||
        "~^:?*[\\/".includes(character)
      );
    }) ||
    encoder.encode(value).byteLength > MAX_PROJECTION_NAME_BYTES
  );
}

function hasPasswordUserinfo(value: string): boolean {
  const scheme = /^[A-Za-z][A-Za-z0-9+.-]*:\/\//.exec(value);
  if (scheme !== null || value.startsWith("//")) {
    const authorityStart = scheme?.[0].length ?? 2;
    const authorityEnd = value.slice(authorityStart).search(/[/?#]/);
    const authority =
      authorityEnd === -1
        ? value.slice(authorityStart)
        : value.slice(authorityStart, authorityStart + authorityEnd);
    const at = authority.lastIndexOf("@");
    return at !== -1 && authority.slice(0, at).includes(":");
  }

  const at = value.indexOf("@");
  if (at === -1) return false;
  const userinfo = value.slice(0, at);
  return !userinfo.includes("/") && userinfo.includes(":");
}
