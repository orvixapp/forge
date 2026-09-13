#!/usr/bin/env node
/**
 * Reproducible, dependency-free first pass over Open VSX extensions.
 *
 * It deliberately uses `unzip` instead of a JavaScript ZIP library so the
 * scanner stays auditable and can run in a clean Node installation. API usage
 * is static evidence only: dynamic property access is reported nowhere rather
 * than guessed.
 */
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";

const REGISTRY = "https://open-vsx.org";

function usage() {
  console.log(`Usage:
  node tools/vscode-api-scan/scan.mjs --input <vsix-directory> --out <report.csv>
  node tools/vscode-api-scan/scan.mjs --vsix <file.vsix> [--vsix <file.vsix> ...] --out <report.csv>
  node tools/vscode-api-scan/scan.mjs --top <count> --out <report.csv>

Options:
  --input <dir>     Recursively scan local .vsix files.
  --vsix <path>     Scan one local .vsix file (repeatable).
  --top <count>     Fetch the most-downloaded Open VSX extensions first.
  --registry <url>  Registry base URL (default: ${REGISTRY}).
  --out <path>      CSV destination (required).
  --help            Show this message.`);
}

function parseArgs(argv) {
  const options = { vsix: [], input: [], registry: REGISTRY, top: 0, out: null };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--help") return { help: true };
    const value = () => {
      index += 1;
      if (!argv[index]) throw new Error(`${flag} requires a value`);
      return argv[index];
    };
    if (flag === "--vsix") options.vsix.push(value());
    else if (flag === "--input") options.input.push(value());
    else if (flag === "--top") {
      options.top = Number.parseInt(value(), 10);
      if (!Number.isSafeInteger(options.top) || options.top < 1) {
        throw new Error("--top must be a positive integer");
      }
    } else if (flag === "--registry") options.registry = value().replace(/\/$/, "");
    else if (flag === "--out") options.out = value();
    else throw new Error(`unknown option ${flag}`);
  }
  if (!options.out) throw new Error("--out is required");
  if (!options.top && !options.vsix.length && !options.input.length) {
    throw new Error("provide --vsix, --input, or --top");
  }
  return options;
}

async function findVsix(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const nested = await Promise.all(entries.map(async (entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return findVsix(path);
    return entry.isFile() && entry.name.endsWith(".vsix") ? [path] : [];
  }));
  return nested.flat();
}

function unzip(vsix, entry) {
  return execFileSync("unzip", ["-p", vsix, entry], { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
}

function zipEntries(vsix) {
  return execFileSync("unzip", ["-Z1", vsix], { encoding: "utf8" })
    .split("\n")
    .filter(Boolean);
}

function csv(value) {
  const text = String(value);
  return /[",\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

function registerUsage(counts, extension, installs, kind, member) {
  const key = `${kind}\u0000${member}`;
  const value = counts.get(key) ?? { kind, member, extensions: 0, installs: 0 };
  value.extensions += 1;
  value.installs += installs;
  counts.set(key, value);
}

function contributedKeys(value) {
  // `contributes.*` is the extension API surface. Descending into settings
  // schemas would turn every configuration property into a fake API member.
  return value && typeof value === "object" && !Array.isArray(value)
    ? Object.keys(value)
    : [];
}

function scanVsix(vsix, installs, counts) {
  let manifest;
  try {
    manifest = JSON.parse(unzip(vsix, "extension/package.json"));
  } catch (error) {
    throw new Error(`${vsix}: cannot read extension/package.json (${error.message})`);
  }
  const extension = `${manifest.publisher ?? "unknown"}.${manifest.name ?? basename(vsix)}`;
  const used = new Set();
  for (const entry of zipEntries(vsix)) {
    if (!/\.(?:c?js|mjs)$/i.test(entry)) continue;
    const source = unzip(vsix, entry);
    for (const match of source.matchAll(/\bvscode\.([A-Za-z_$][\w$]*)(?:\.([A-Za-z_$][\w$]*))?/g)) {
      used.add(`${match[1]}.${match[2] ?? "*"}`);
    }
  }
  for (const member of used) registerUsage(counts, extension, installs, "api", member);
  for (const member of new Set(contributedKeys(manifest.contributes))) {
    registerUsage(counts, extension, installs, "contributes", member);
  }
  return { extension, hasMain: Boolean(manifest.main || manifest.browser), installs };
}

async function fetchTop(registry, count, target) {
  const discovered = [];
  for (let offset = 0; discovered.length < count; offset += 50) {
    const url = new URL("/api/-/search", registry);
    url.searchParams.set("size", "50");
    url.searchParams.set("offset", String(offset));
    url.searchParams.set("sortBy", "downloadCount");
    url.searchParams.set("sortOrder", "desc");
    const response = await fetch(url);
    if (!response.ok) throw new Error(`Open VSX search failed: ${response.status} ${response.statusText}`);
    const page = await response.json();
    const extensions = page.extensions ?? [];
    if (!extensions.length) break;
    discovered.push(...extensions);
  }
  const downloads = [];
  for (const extension of discovered.slice(0, count)) {
    const name = `${extension.namespace}.${extension.name}-${extension.version}.vsix`;
    const destination = join(target, name);
    const download = extension.files?.download
      ?? `${registry}/api/${extension.namespace}/${extension.name}/${extension.version}/file/${name}`;
    const response = await fetch(download);
    if (!response.ok) throw new Error(`cannot download ${extension.namespace}.${extension.name}: ${response.status}`);
    await writeFile(destination, new Uint8Array(await response.arrayBuffer()));
    downloads.push({ path: destination, installs: Number(extension.downloadCount) || 0 });
  }
  return downloads;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) return usage();
  const local = (await Promise.all(options.input.map(findVsix))).flat().map((path) => ({ path, installs: 0 }));
  local.push(...options.vsix.map((path) => ({ path, installs: 0 })));
  let temporary;
  try {
    if (options.top) {
      temporary = await mkdtemp(join(tmpdir(), "forge-openvsx-"));
      local.push(...await fetchTop(options.registry, options.top, temporary));
    }
    const counts = new Map();
    const scanned = local.map(({ path, installs }) => scanVsix(path, installs, counts));
    const rows = [...counts.values()].sort((a, b) => b.installs - a.installs || b.extensions - a.extensions || a.member.localeCompare(b.member));
    await mkdir(dirname(options.out), { recursive: true });
    await writeFile(options.out, ["kind,member,extensions,installs", ...rows.map((row) => [row.kind, row.member, row.extensions, row.installs].map(csv).join(","))].join("\n") + "\n");
    const tierZero = scanned.filter(({ hasMain }) => !hasMain).length;
    console.log(JSON.stringify({ scanned: scanned.length, tierZero, rows: rows.length, out: options.out }, null, 2));
  } finally {
    if (temporary) await rm(temporary, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(`vscode-api-scan: ${error.message}`);
  process.exitCode = 1;
});
