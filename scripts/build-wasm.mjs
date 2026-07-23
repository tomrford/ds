import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, statSync } from "node:fs";

buildWasm({
  packageName: "devspace-kernel-wasm",
  artifactName: "devspace_kernel_wasm.wasm",
  outputName: "kernel.wasm",
  budget: 200 * 1024,
});

buildWasm({
  packageName: "devspace-kernel-git-wasm",
  artifactName: "devspace_kernel_git_wasm.wasm",
  outputName: "kernel-git.wasm",
  budget: 200 * 1024,
});

function buildWasm({ packageName, artifactName, outputName, budget }) {
  run("cargo", [
    "build",
    "--profile",
    "wasm-release",
    "--target",
    "wasm32-unknown-unknown",
    "-p",
    packageName,
  ]);

  mkdirSync("dist", { recursive: true });
  const source = `target/wasm32-unknown-unknown/wasm-release/${artifactName}`;
  const output = `dist/${outputName}`;
  run("wasm-opt", [
    "-Oz",
    "--enable-bulk-memory",
    "--enable-bulk-memory-opt",
    source,
    "-o",
    output,
  ]);

  const wasmBytes = statSync(output).size;
  const imports = WebAssembly.Module.imports(new WebAssembly.Module(readFileSync(output)));
  if (imports.length !== 0) {
    throw new Error(`${output} has ${imports.length} WebAssembly imports`);
  }
  console.log(`${output}: ${wasmBytes} bytes, zero imports`);
  if (budget !== undefined && wasmBytes > budget) {
    throw new Error(`optimized validation kernel is ${wasmBytes} bytes; budget is ${budget}`);
  }
}

function run(command, arguments_) {
  const result = spawnSync(command, arguments_, { stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
