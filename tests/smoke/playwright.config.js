// @ts-check
import { defineConfig, devices } from "@playwright/test"

// The site as CI assembles it: `web/` plus the fixtures and the
// static twin of the read API. Served over HTTP rather than opened as a
// `file://` URL, because the page fetches `./api/sessions.json` and a file URL
// makes that a cross-origin request the browser refuses.
export default defineConfig({
  testDir: ".",
  // The live-server suite has its own config and its own server: `gate.config.js`.
  testMatch: "site.spec.js",
  timeout: 30_000,
  // A smoke test that passes on the second attempt is a smoke test that failed.
  retries: 0,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: "http://127.0.0.1:4173",
    trace: "retain-on-failure",
  },
  webServer: {
    command: "python3 -m http.server 4173 --bind 127.0.0.1 --directory ../../site",
    url: "http://127.0.0.1:4173/index.html",
    reuseExistingServer: !process.env.CI,
    stdout: "ignore",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
})
