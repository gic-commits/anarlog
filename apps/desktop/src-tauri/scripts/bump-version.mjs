// Increments the patch version in tauri.conf.json before every bundle.
// Uses an environment override (BUILD_VERSION) when provided, otherwise bumps
// the stored patch (0.0.1 -> 0.0.2 -> ...). Running twice in a row always
// advances, so each `tauri build` gets a fresh version.
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const confPath = path.join(scriptDir, "..", "tauri.conf.json");

const conf = JSON.parse(readFileSync(confPath, "utf8"));

let next;
const override = process.env.BUILD_VERSION;
if (override) {
  next = override;
} else {
  const current = conf.version || "0.0.0";
  const parts = String(current).split(".").map((p) => Number.parseInt(p, 10) || 0);
  parts[2] = (parts[2] || 0) + 1;
  next = parts.join(".");
}

conf.version = next;
writeFileSync(confPath, JSON.stringify(conf, null, 2) + "\n");
console.log(`[bump-version] version -> ${next}`);
