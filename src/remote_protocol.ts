import { MAX_PROJECTION_NAME_BYTES } from "./projection_protocol";

export const MAX_REMOTE_REQUEST_BYTES = 2 * 1024;
export const MAX_REMOTE_URL_BYTES = 1024;

const SHORT_ID_PATTERN = /^[0-9a-f]{32}$/;
const encoder = new TextEncoder();

export interface SetRemoteRequest {
  incarnation: Uint8Array;
  url: string;
}

export class RemoteProtocolError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
  }
}

export function decodeRemoteName(value: unknown): string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    [...value].some((character) => {
      const code = character.codePointAt(0) ?? 0;
      return code < 0x20 || code === 0x7f || (code >= 0xd800 && code <= 0xdfff);
    })
  ) {
    throw new RemoteProtocolError(
      "remote name must be a non-empty string without control characters",
      "invalid-remote-name",
    );
  }
  if (encoder.encode(value).byteLength > MAX_PROJECTION_NAME_BYTES) {
    throw new RemoteProtocolError(
      `remote name exceeds ${MAX_PROJECTION_NAME_BYTES} UTF-8 bytes`,
      "invalid-remote-name",
    );
  }
  return value;
}

export function decodeSetRemote(value: unknown): SetRemoteRequest {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new RemoteProtocolError(
      "remote request must be a JSON object",
      "invalid-remote-request",
    );
  }
  const record = value as Record<string, unknown>;
  const actual = Object.keys(record).sort();
  const expected = ["incarnation", "url"];
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new RemoteProtocolError(
      "remote request fields must be exactly incarnation, url",
      "invalid-remote-request",
    );
  }
  if (typeof record.incarnation !== "string" || !SHORT_ID_PATTERN.test(record.incarnation)) {
    throw new RemoteProtocolError(
      "incarnation must be 32 lowercase hex characters",
      "invalid-incarnation",
    );
  }
  return {
    incarnation: decodeHex(record.incarnation),
    url: decodeRemoteUrl(record.url),
  };
}

export function decodeRemoteIncarnation(value: unknown): Uint8Array {
  if (typeof value !== "string" || !SHORT_ID_PATTERN.test(value)) {
    throw new RemoteProtocolError(
      "incarnation must be 32 lowercase hex characters",
      "invalid-incarnation",
    );
  }
  return decodeHex(value);
}

function decodeRemoteUrl(value: unknown): string {
  if (typeof value !== "string" || value.length === 0 || /[\r\n\u0085\u2028\u2029]/.test(value)) {
    throw new RemoteProtocolError(
      "remote URL must be a non-empty single-line string",
      "invalid-remote-url",
    );
  }
  if (encoder.encode(value).byteLength > MAX_REMOTE_URL_BYTES) {
    throw new RemoteProtocolError(
      `remote URL exceeds ${MAX_REMOTE_URL_BYTES} UTF-8 bytes`,
      "invalid-remote-url",
    );
  }
  if (hasPasswordUserinfo(value)) {
    throw new RemoteProtocolError(
      "remote URL must not contain userinfo credentials",
      "credentials-in-remote-url",
    );
  }
  return value;
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

function decodeHex(value: string): Uint8Array {
  return Uint8Array.from({ length: 16 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}
