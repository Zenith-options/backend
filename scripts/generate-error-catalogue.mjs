import { readFile, writeFile } from "node:fs/promises";

const entries = JSON.parse(await readFile("docs/error-codes.json", "utf8"));
const lines = [
  "# API error catalogue",
  "",
  "This file is generated from `docs/error-codes.json`. Error codes are stable API identifiers; clients must not branch on message text.",
  "",
  "## Adding a new error code",
  "",
  "1. Add the new code to the source that raises it.",
  "2. Add a matching entry (`code`, `http_status`, `description`) to `docs/error-codes.json`.",
  "3. Regenerate this catalogue by running `node scripts/generate-error-catalogue.mjs` from the repository root.",
  "4. Commit both `docs/error-codes.json` and the regenerated `docs/error-catalogue.md` together.",
  "",
  "| Code | HTTP status | Meaning |",
  "|---|---:|---|",
  ...entries.map(({ code, http_status, description }) =>
    `| \`${code}\` | ${Array.isArray(http_status) ? http_status.join(", ") : http_status} | ${description} |`,
  ),
  "",
];
await writeFile("docs/error-catalogue.md", lines.join("\n"));
