# Open VSX API scan

This is the Phase 0 evidence collector for Forge's VS Code compatibility
layer. It does not run extensions and it does not prove compatibility. It
answers the narrower, useful question: which declarative contributions and
statically visible `vscode` members are most important in the sampled Open VSX
catalogue, weighted by download count.

The scanner writes a CSV compatibility input table:

```text
kind,member,extensions,installs
api,workspace.openTextDocument,42,1200000
contributes,commands,42,1200000
```

It has no npm dependencies. It needs Node 18+ (for `fetch`) and `unzip`.

## Reproducible Phase 0 run

Run the registry scan from the repository root and retain both the CSV and the
terminal JSON summary in the dated Phase 0 evidence directory:

```bash
mkdir -p bench/results/openvsx-$(date +%F)
date --iso-8601=seconds > bench/results/openvsx-$(date +%F)/metadata.txt
node --version >> bench/results/openvsx-$(date +%F)/metadata.txt
node tools/vscode-api-scan/scan.mjs --top 1000 \
  --out bench/results/openvsx-$(date +%F)/api-usage.csv \
  | tee bench/results/openvsx-$(date +%F)/summary.json
sha256sum bench/results/openvsx-$(date +%F)/api-usage.csv \
  >> bench/results/openvsx-$(date +%F)/metadata.txt
```

The registry is live, so the date, Node version and CSV checksum are part of
the result. Do not overwrite a prior dated run: comparison between snapshots
is evidence of catalogue drift.

Scan downloaded VSIX files without accessing the network:

```bash
node tools/vscode-api-scan/scan.mjs --input /path/to/vsix --out bench/results/openvsx-api.csv
```

Fetch and scan the top 1,000 extensions from Open VSX:

```bash
node tools/vscode-api-scan/scan.mjs --top 1000 --out bench/results/openvsx-api.csv
```

## What the CSV means

- `kind` is `api` for a `vscode.namespace.member` reference, or `contributes`
  for a top-level `package.json#contributes` key.
- `member` is the observed member. A namespace accessed without a visible
  member is emitted as `namespace.*`.
- `extensions` is the number of scanned extensions that use that member once
  or more; a member is not counted twice within one extension.
- `installs` is the sum of the Open VSX download counts for those extensions.
  It ranks impact; it is not a count of active users.

## Limits and use

The scan intentionally uses a dependency-free regular-expression pass over
JavaScript bundles. It sees static `vscode.namespace.member` access and the
manifest's top-level contributions; it does not resolve aliases, imports,
computed properties, generated code, runtime activation, or semantic API
usage. Dynamic access is excluded rather than guessed.

Use the highest-impact rows as the initial Tier 0/1 backlog, then verify each
candidate against `vscode.d.ts` and a real-extension conformance test before
claiming support. The full Phase 0 acceptance checklist is in
[`docs/PHASE_0.md`](../../docs/PHASE_0.md).
