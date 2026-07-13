#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";
import { validateNodeMajor } from "./chp-runtime.mjs";

const expectedVersion = "5.3.0";
const requestedSource = process.env.CHP_SOURCE_DIR || "/opt/chp-5.3.0";
const source = fs.realpathSync(requestedSource);
if (source.split(path.sep).includes("node_modules")) {
  throw new Error("refusing global node_modules source; use the pinned in-image CHP tree");
}
const packagePath = path.join(source, "package.json");
const configProxyPath = path.join(source, "lib", "configproxy.js");
const cliPath = path.join(source, "bin", "configurable-http-proxy");

for (const required of [packagePath, configProxyPath, cliPath]) {
  const resolved = fs.realpathSync(required);
  if (resolved !== source && !resolved.startsWith(`${source}${path.sep}`)) {
    throw new Error(`CHP source member escaped the pinned source directory: ${required}`);
  }
}

const packageJson = JSON.parse(fs.readFileSync(packagePath, "utf8"));
if (
  packageJson.name !== "configurable-http-proxy" ||
  packageJson.version !== expectedVersion
) {
  throw new Error(
    `expected configurable-http-proxy ${expectedVersion}, got ${packageJson.name} ${packageJson.version}`
  );
}

validateNodeMajor(process.version);

if (process.argv[2] === "--runtime-probe") {
  process.stdout.write(
    `${JSON.stringify({
      node: process.version,
      package: `${packageJson.name}@${packageJson.version}`,
      source,
    })}\n`
  );
  process.exit(0);
}

// Import the requested oracle implementation itself before executing its
// pinned CLI in this process. This intentionally cannot resolve a global Node
// package and leaves no descendant process for the Rust harness to reap.
await import(pathToFileURL(configProxyPath).href);
process.argv[1] = cliPath;
await import(pathToFileURL(cliPath).href);
