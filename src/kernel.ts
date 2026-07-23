import kernelModule from "../dist/kernel.wasm";

export const GIT_OBJECT_KIND = {
  blob: 0,
  tree: 1,
  commit: 2,
} as const;

export const GIT_REFERENCE_KIND = {
  blob: 0,
  executable: 1,
  symlink: 2,
  tree: 3,
  commit: 4,
  gitlink: 5,
} as const;

export const OP_REFERENCE_KIND = {
  commit: 0,
  view: 1,
  operation: 2,
} as const;

interface KernelExports extends WebAssembly.Exports {
  memory: WebAssembly.Memory;
  kernel_alloc(length: number): number;
  kernel_dealloc(pointer: number, length: number): void;
  kernel_validate(kind: number, pointer: number, length: number): bigint;
  kernel_validate_view(pointer: number, length: number): bigint;
  kernel_validate_operation(pointer: number, length: number): bigint;
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
    return this.call(bytes, (pointer, length) =>
      this.exports.kernel_validate(kind, pointer, length),
    );
  }

  validateView(bytes: Uint8Array): KernelResult {
    return decodeOpKernelResult(
      this.callRaw(bytes, (pointer, length) =>
        this.exports.kernel_validate_view(pointer, length),
      ),
    );
  }

  validateOperation(bytes: Uint8Array): KernelResult {
    return decodeOpKernelResult(
      this.callRaw(bytes, (pointer, length) =>
        this.exports.kernel_validate_operation(pointer, length),
      ),
    );
  }

  private call(
    bytes: Uint8Array,
    validate: (pointer: number, length: number) => bigint,
  ): KernelResult {
    return decodeKernelResult(this.callRaw(bytes, validate));
  }

  private callRaw(
    bytes: Uint8Array,
    validate: (pointer: number, length: number) => bigint,
  ): Uint8Array {
    const inputPointer = this.exports.kernel_alloc(bytes.byteLength);
    try {
      new Uint8Array(this.exports.memory.buffer, inputPointer, bytes.byteLength).set(bytes);
      const packed = validate(inputPointer, bytes.byteLength);
      const outputPointer = Number(packed & 0xffff_ffffn);
      const outputLength = Number(packed >> 32n);
      try {
        const output = new Uint8Array(
          this.exports.memory.buffer,
          outputPointer,
          outputLength,
        ).slice();
        return output;
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
  if (bytes[0] === 1) throw new Error(new TextDecoder().decode(bytes.subarray(1)));
  if (bytes[0] !== 0 || bytes.byteLength < 25) {
    throw new Error("Git validation kernel returned a malformed response");
  }
  const count = new DataView(bytes.buffer, bytes.byteOffset + 21, 4).getUint32(0, true);
  if (bytes.byteLength !== 25 + count * 21) {
    throw new Error("Git validation kernel returned malformed references");
  }
  const references = [];
  for (let index = 0; index < count; index += 1) {
    const offset = 25 + index * 21;
    const kind = bytes[offset];
    if (kind > GIT_REFERENCE_KIND.gitlink) {
      throw new Error("Git validation kernel returned an unknown reference kind");
    }
    references.push({ kind, id: bytes.slice(offset + 1, offset + 21) });
  }
  return { id: bytes.slice(1, 21), references };
}

function decodeOpKernelResult(bytes: Uint8Array): KernelResult {
  if (bytes[0] === 1) throw new Error(new TextDecoder().decode(bytes.subarray(1)));
  if (bytes[0] !== 0 || bytes.byteLength < 69) {
    throw new Error("operation-store validation kernel returned a malformed response");
  }
  const count = new DataView(bytes.buffer, bytes.byteOffset + 65, 4).getUint32(0, true);
  const references = [];
  let offset = 69;
  for (let index = 0; index < count; index += 1) {
    const kind = bytes[offset];
    const idLength =
      kind === OP_REFERENCE_KIND.commit
        ? 20
        : kind === OP_REFERENCE_KIND.view || kind === OP_REFERENCE_KIND.operation
          ? 64
          : undefined;
    if (idLength === undefined || offset + 1 + idLength > bytes.byteLength) {
      throw new Error("operation-store validation kernel returned malformed references");
    }
    references.push({ kind, id: bytes.slice(offset + 1, offset + 1 + idLength) });
    offset += 1 + idLength;
  }
  if (offset !== bytes.byteLength) {
    throw new Error("operation-store validation kernel returned trailing response bytes");
  }
  return { id: bytes.slice(1, 65), references };
}

export function equalGitBytes(left: Uint8Array, right: Uint8Array): boolean {
  return left.byteLength === right.byteLength && left.every((byte, index) => byte === right[index]);
}

export function compareGitBytes(left: Uint8Array, right: Uint8Array): number {
  const length = Math.min(left.byteLength, right.byteLength);
  for (let index = 0; index < length; index += 1) {
    const difference = left[index] - right[index];
    if (difference !== 0) return difference;
  }
  return left.byteLength - right.byteLength;
}

export function exactGitBuffer(bytes: Uint8Array): ArrayBuffer {
  return bytes.slice().buffer as ArrayBuffer;
}

export function gitToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function gitHashFromHex(value: string): Uint8Array {
  if (!/^[0-9a-f]{128}$/.test(value)) {
    throw new Error("hash must be 128 lowercase hex characters");
  }
  return decodeHex(value, 64);
}

function decodeHex(value: string, length: number): Uint8Array {
  return Uint8Array.from({ length }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}
