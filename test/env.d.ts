import type { DevelopmentSecretEnv } from "../src/auth";

declare global {
  namespace Cloudflare {
    interface Env extends DevelopmentSecretEnv {}
  }
}
