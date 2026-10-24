import { readFileSync } from "node:fs";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
const source = read("src/lib.rs");
const jsonSpec = JSON.parse(read("openapi.json"));
const yamlSpec = read("openapi/openapi.yaml");
const required = [
  "/api/v1/keeper/bounties",
  "/api/v1/privacy/export",
  "/api/v1/privacy/erasure",
  "/api/v1/settlement/statements",
];
const routes = new Set(
  [...source.matchAll(/\.route\(\s*"([^"]+)"/gs)].map((match) => match[1]),
);

if (!source.includes("api_version::negotiate_version")) {
  throw new Error("API version negotiation middleware is not installed on the router");
}

const missing = required.filter(
  (route) => !routes.has(route) || !Object.hasOwn(jsonSpec.paths, route) || !yamlSpec.includes(`\n  ${route}:`),
);
if (missing.length) {
  throw new Error(`Settlement API route/spec mismatch: ${missing.join(", ")}`);
}
