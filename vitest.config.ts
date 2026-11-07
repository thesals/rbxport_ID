import { defineConfig } from "vitest/config";
import { fileURLToPath, URL } from "node:url";

export default defineConfig({
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  test: {
    environment: "node",
    // `.tsx` for the few tests that mount a component. Those opt into jsdom
    // with a docblock of their own, so everything else stays in node.
    include: ["src/**/*.test.ts", "src/**/*.test.tsx", "design/**/*.test.ts", "scripts/cleanup.test.mjs"],
    reporters: "dot",
    // Node 25+ has a global `localStorage` of its own, `undefined` without
    // `--localstorage-file`, which can stand in front of jsdom's. Off, so a
    // jsdom test gets jsdom's storage on every Node version (Node 24 accepts
    // the same flag).
    poolOptions: {
      forks: { execArgv: ["--no-experimental-webstorage"] },
      threads: { execArgv: ["--no-experimental-webstorage"] },
    },
  },
});
