import { readFile, writeFile } from "node:fs/promises";

const entries = JSON.parse(await readFile("docs/error-codes.json", "utf8"));
const lines = [
  "# API error catalogue",
  "",
  "This file is generated from `docs/error-codes.json`. Error codes are stable API identifiers; clients must not branch on message text.",
  "",
  "| Code | HTTP status | Meaning |",
  "|---|---:|---|",
  ...entries.map(({ code, status, http_status, description }) => {
    const resolved = status ?? http_status;
    const rendered = Array.isArray(resolved) ? resolved.join(", ") : resolved;
    return `| \`${code}\` | ${rendered} | ${description} |`;
  }),
  "",
];
await writeFile("docs/error-catalogue.md", lines.join("\n"));
