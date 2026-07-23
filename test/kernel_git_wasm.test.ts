import { expect, it } from "vitest";
import gitKernelModule from "../dist/kernel-git.wasm";
import gitGolden from "../crates/kernel-git/tests/git_golden.txt?raw";
import gitGoldenOracle from "../crates/kernel-git/tests/git_golden_oracle.txt?raw";
import opsGolden from "../crates/kernel-git/tests/ops_golden.txt?raw";
import { KernelGit, OP_REFERENCE_KIND } from "../src/kernel_git";

interface GitKernelExports extends WebAssembly.Exports {
  memory: WebAssembly.Memory;
  kernel_alloc(length: number): number;
  kernel_dealloc(pointer: number, length: number): void;
  kernel_validate(kind: number, pointer: number, length: number): bigint;
}

const kindByName = { blob: 0, tree: 1, commit: 2 } as const;

it("matches native Git IDs and acceptance for all 40 vectors through Wasm", () => {
  const exports = new WebAssembly.Instance(gitKernelModule, {}).exports as GitKernelExports;
  const lines = `${gitGolden}\n${gitGoldenOracle}`
    .split("\n")
    .filter((line) => line !== "" && !line.startsWith("#"));
  expect(lines).toHaveLength(40);

  for (const line of lines) {
    const [kindName, expectedId, payloadHex] = line.split("|");
    if (!(kindName in kindByName)) throw new Error(`unknown Git object kind ${kindName}`);
    const result = validate(
      exports,
      kindByName[kindName as keyof typeof kindByName],
      decodeHex(payloadHex),
    );
    expect(encodeHex(result), `${kindName} ID`).toBe(expectedId);
  }
});

it("matches ported operation IDs and enforces 20-byte Git view references through Wasm", () => {
  const kernel = new KernelGit();
  const operations = opsGolden
    .split("\n")
    .filter((line) => line !== "" && !line.startsWith("#"));
  expect(operations).toHaveLength(8);
  for (const line of operations) {
    const [kind, expectedId, payloadRle] = line.split("|");
    const bytes = decodeRle(payloadRle);
    const validated =
      kind === "view"
        ? kernel.validateView(bytes)
        : kind === "operation"
          ? kernel.validateOperation(bytes)
          : undefined;
    if (validated === undefined) throw new Error(`unknown operation-store kind ${kind}`);
    expect(encodeHex(validated.id)).toBe(expectedId);
  }

  const view = new Uint8Array([
    0x0a,
    20,
    ...new Uint8Array(20).fill(1),
    0x4a,
    4,
    0x1a,
    2,
    0x12,
    0,
    0x60,
    1,
  ]);
  const validated = kernel.validateView(view);
  expect(validated.id).toHaveLength(64);
  expect(validated.references).toEqual([
    { kind: OP_REFERENCE_KIND.commit, id: new Uint8Array(20).fill(1) },
  ]);
});

function validate(exports: GitKernelExports, kind: number, bytes: Uint8Array): Uint8Array {
  const inputPointer = exports.kernel_alloc(bytes.byteLength);
  try {
    new Uint8Array(exports.memory.buffer, inputPointer, bytes.byteLength).set(bytes);
    const packed = exports.kernel_validate(kind, inputPointer, bytes.byteLength);
    const outputPointer = Number(packed & 0xffff_ffffn);
    const outputLength = Number(packed >> 32n);
    try {
      const output = new Uint8Array(
        exports.memory.buffer,
        outputPointer,
        outputLength,
      ).slice();
      if (output[0] === 1) {
        throw new Error(new TextDecoder().decode(output.subarray(1)));
      }
      if (output[0] !== 0 || output.byteLength < 25) {
        throw new Error("Git validation kernel returned a malformed response");
      }
      const referenceCount = new DataView(output.buffer, output.byteOffset + 21, 4).getUint32(
        0,
        true,
      );
      if (output.byteLength !== 25 + referenceCount * 21) {
        throw new Error("Git validation kernel returned malformed references");
      }
      return output.slice(1, 21);
    } finally {
      exports.kernel_dealloc(outputPointer, outputLength);
    }
  } finally {
    exports.kernel_dealloc(inputPointer, bytes.byteLength);
  }
}

function decodeHex(value: string): Uint8Array {
  if (value.length % 2 !== 0) throw new Error("odd-length hex payload");
  return Uint8Array.from({ length: value.length / 2 }, (_, index) =>
    Number.parseInt(value.slice(index * 2, index * 2 + 2), 16),
  );
}

function encodeHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function decodeRle(value: string): Uint8Array {
  return Uint8Array.from(
    value.split(",").flatMap((run) => {
      const [byte, count] = run.split("*");
      return Array(Number.parseInt(count, 10)).fill(Number.parseInt(byte, 16));
    }),
  );
}
