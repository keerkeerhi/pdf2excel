import { readFileSync, writeFileSync } from "node:fs";

const packageJson = JSON.parse(readFileSync("package.json", "utf8"));
const version = packageJson.version;

if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`Invalid package.json version: ${version}`);
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function writeJson(path, value) {
  const indent = path === "src-tauri/tauri.conf.json" ? 4 : 2;
  writeFileSync(path, `${JSON.stringify(value, null, indent)}\n`);
}

const packageLock = readJson("package-lock.json");
packageLock.version = version;
packageLock.packages[""].version = version;
writeJson("package-lock.json", packageLock);

const tauriConfig = readJson("src-tauri/tauri.conf.json");
tauriConfig.version = version;
writeJson("src-tauri/tauri.conf.json", tauriConfig);

const cargoToml = readFileSync("src-tauri/Cargo.toml", "utf8");
const cargoTomlPattern = /(^\[package\][\s\S]*?^version = ")[^"]+("\s*$)/m;
if (!cargoTomlPattern.test(cargoToml)) throw new Error("Could not find the [package] version in src-tauri/Cargo.toml");
const updatedCargoToml = cargoToml.replace(
  cargoTomlPattern,
  (_match, prefix, suffix) => `${prefix}${version}${suffix}`,
);
writeFileSync("src-tauri/Cargo.toml", updatedCargoToml);

const cargoLock = readFileSync("src-tauri/Cargo.lock", "utf8");
const cargoLockPattern = /(^name = "pdf-table-workshop"\r?\nversion = ")[^"]+("\r?$)/m;
if (!cargoLockPattern.test(cargoLock)) throw new Error("Could not find pdf-table-workshop in src-tauri/Cargo.lock");
const updatedCargoLock = cargoLock.replace(
  cargoLockPattern,
  (_match, prefix, suffix) => `${prefix}${version}${suffix}`,
);
writeFileSync("src-tauri/Cargo.lock", updatedCargoLock);

console.log(`Synchronized project versions to ${version}`);
