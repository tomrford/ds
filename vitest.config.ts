import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      miniflare: {
        bindings: { SPIKE_TOKEN: "test-token" },
      },
      wrangler: { configPath: "./wrangler.jsonc" },
    }),
  ],
});

