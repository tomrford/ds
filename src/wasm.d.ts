declare module "*.wasm" {
  const module: WebAssembly.Module;
  export default module;
}

declare module "*?raw" {
  const contents: string;
  export default contents;
}
