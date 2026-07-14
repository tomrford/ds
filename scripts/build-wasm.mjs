import { spawnSync } from "node:child_process";
import { mkdirSync, statSync } from "node:fs";

run("cargo", [
  "build",
  "--profile",
  "wasm-release",
  "--target",
  "wasm32-unknown-unknown",
  "-p",
  "devspace-kernel-wasm",
]);

mkdirSync("dist", { recursive: true });
run("wasm-opt", [
  "-Oz",
  "--enable-bulk-memory",
  "--enable-bulk-memory-opt",
  "target/wasm32-unknown-unknown/wasm-release/devspace_kernel_wasm.wasm",
  "-o",
  "dist/kernel.wasm",
]);

const wasmBytes = statSync("dist/kernel.wasm").size;
if (wasmBytes > 200 * 1024) {
  throw new Error(`optimized validation kernel is ${wasmBytes} bytes; budget is 204800`);
}

function run(command, arguments_) {
  const result = spawnSync(command, arguments_, { stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
