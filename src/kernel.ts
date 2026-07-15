import kernelModule from "../dist/kernel.wasm";

export const KIND = {
  file: 0,
  symlink: 1,
  tree: 2,
  commit: 3,
  view: 4,
  operation: 5,
} as const;

export const KIND_BY_NUMBER = [
  "file",
  "symlink",
  "tree",
  "commit",
  "view",
  "operation",
] as const;

export type KindName = keyof typeof KIND;

export function isKindName(value: string): value is KindName {
  return Object.hasOwn(KIND, value);
}

interface KernelExports extends WebAssembly.Exports {
  memory: WebAssembly.Memory;
  kernel_alloc(length: number): number;
  kernel_dealloc(pointer: number, length: number): void;
  kernel_validate(kind: number, pointer: number, length: number): bigint;
  kernel_hash_new(): number;
  kernel_hash_update(state: number, pointer: number, length: number): void;
  kernel_hash_finish(state: number): number;
  kernel_hash_drop(state: number): void;
}

export interface KernelResult {
  id: Uint8Array;
  references: Array<{ kind: number; id: Uint8Array }>;
}

export class Kernel {
  private exports: KernelExports;

  constructor() {
    this.exports = instantiate();
  }

  reset() {
    this.exports = instantiate();
  }

  validate(kind: number, bytes: Uint8Array): KernelResult {
    const inputPointer = this.exports.kernel_alloc(bytes.byteLength);
    try {
      new Uint8Array(this.exports.memory.buffer, inputPointer, bytes.byteLength).set(bytes);
      const packed = this.exports.kernel_validate(kind, inputPointer, bytes.byteLength);
      const outputPointer = Number(packed & 0xffff_ffffn);
      const outputLength = Number(packed >> 32n);
      try {
        const output = new Uint8Array(
          this.exports.memory.buffer,
          outputPointer,
          outputLength,
        ).slice();
        return decodeKernelResult(output);
      } finally {
        this.exports.kernel_dealloc(outputPointer, outputLength);
      }
    } finally {
      this.exports.kernel_dealloc(inputPointer, bytes.byteLength);
    }
  }

  startHash(): KernelHash {
    return new KernelHash(this.exports);
  }

  hash(parts: Iterable<Uint8Array>): Uint8Array {
    const hash = this.startHash();
    try {
      for (const part of parts) hash.update(part);
      return hash.finish();
    } finally {
      hash.dispose();
    }
  }
}

export class KernelHash {
  private state: number | undefined;

  constructor(private readonly exports: KernelExports) {
    this.state = exports.kernel_hash_new();
  }

  update(bytes: Uint8Array) {
    if (this.state === undefined) throw new Error("hash state is already finished");
    const pointer = this.exports.kernel_alloc(bytes.byteLength);
    try {
      new Uint8Array(this.exports.memory.buffer, pointer, bytes.byteLength).set(bytes);
      this.exports.kernel_hash_update(this.state, pointer, bytes.byteLength);
    } finally {
      this.exports.kernel_dealloc(pointer, bytes.byteLength);
    }
  }

  finish(): Uint8Array {
    if (this.state === undefined) throw new Error("hash state is already finished");
    const state = this.state;
    this.state = undefined;
    const pointer = this.exports.kernel_hash_finish(state);
    try {
      return new Uint8Array(this.exports.memory.buffer, pointer, 64).slice();
    } finally {
      this.exports.kernel_dealloc(pointer, 64);
    }
  }

  dispose() {
    if (this.state !== undefined) {
      this.exports.kernel_hash_drop(this.state);
      this.state = undefined;
    }
  }
}

function instantiate(): KernelExports {
  return new WebAssembly.Instance(kernelModule, {}).exports as KernelExports;
}

function decodeKernelResult(bytes: Uint8Array): KernelResult {
  if (bytes[0] === 1) {
    throw new Error(new TextDecoder().decode(bytes.subarray(1)));
  }
  if (bytes[0] !== 0 || bytes.byteLength < 69) {
    throw new Error("validation kernel returned a malformed response");
  }
  const count = new DataView(bytes.buffer, bytes.byteOffset + 65, 4).getUint32(0, true);
  if (bytes.byteLength !== 69 + count * 65) {
    throw new Error("validation kernel returned malformed references");
  }
  const references = [];
  for (let index = 0; index < count; index += 1) {
    const offset = 69 + index * 65;
    const kind = bytes[offset];
    if (KIND_BY_NUMBER[kind] === undefined) {
      throw new Error("validation kernel returned an unknown reference kind");
    }
    references.push({ kind, id: bytes.slice(offset + 1, offset + 65) });
  }
  return { id: bytes.slice(1, 65), references };
}

export function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  return left.byteLength === right.byteLength && left.every((byte, index) => byte === right[index]);
}

export function compareBytes(left: Uint8Array, right: Uint8Array): number {
  const shared = Math.min(left.byteLength, right.byteLength);
  for (let index = 0; index < shared; index += 1) {
    const difference = left[index] - right[index];
    if (difference !== 0) return difference;
  }
  return left.byteLength - right.byteLength;
}

export function exactBuffer(bytes: Uint8Array): ArrayBuffer {
  return new Uint8Array(bytes).buffer;
}

export function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function fromHex(value: string): Uint8Array {
  if (!/^[0-9a-f]{128}$/.test(value)) throw new Error("ID must be 128 lowercase hex characters");
  return Uint8Array.from({ length: 64 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}
