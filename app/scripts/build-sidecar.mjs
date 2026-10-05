// Builds the vendored extract-xiso (third_party/extract-xiso) and places it where Tauri looks
// for sidecar binaries: src-tauri/binaries/extract-xiso-<target triple>[.exe].
//
// Runs before `tauri dev` and `tauri build`. Needs CMake and a C compiler (Xcode command line
// tools on macOS, Visual Studio Build Tools on Windows). Skips the build when the binary is
// newer than the source.
//
// The target triple comes from TAURI_ENV_TARGET_TRIPLE when Tauri sets it (cross builds,
// e.g. `tauri build --target x86_64-apple-darwin`), else from rustc's host.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../..");
const src = path.join(root, "third_party", "extract-xiso");

const triple =
  process.env.TAURI_ENV_TARGET_TRIPLE ||
  execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (\S+)$/m)[1];
const exe = triple.includes("windows") ? ".exe" : "";
const outDir = path.join(root, "app", "src-tauri", "binaries");
const out = path.join(outDir, `extract-xiso-${triple}${exe}`);
const build = path.join(root, "target", "extract-xiso", triple);

const srcTime = Math.max(
  ...["extract-xiso.c", "CMakeLists.txt"].map((f) => fs.statSync(path.join(src, f)).mtimeMs),
);
if (fs.existsSync(out) && fs.statSync(out).mtimeMs > srcTime) {
  console.log(`extract-xiso: ${path.relative(root, out)} is up to date`);
  process.exit(0);
}

const configure = ["-S", src, "-B", build, "-DCMAKE_BUILD_TYPE=Release"];
if (triple.startsWith("aarch64-apple-darwin")) configure.push("-DCMAKE_OSX_ARCHITECTURES=arm64");
if (triple.startsWith("x86_64-apple-darwin")) configure.push("-DCMAKE_OSX_ARCHITECTURES=x86_64");
if (triple.includes("apple-darwin")) configure.push("-DCMAKE_OSX_DEPLOYMENT_TARGET=11.0");
if (triple.startsWith("aarch64-pc-windows")) configure.push("-A", "ARM64");
if (triple.startsWith("x86_64-pc-windows")) configure.push("-A", "x64");

const run = (args) => execFileSync("cmake", args, { stdio: "inherit" });
console.log(`extract-xiso: building for ${triple}`);
run(configure);
run(["--build", build, "--config", "Release"]);

const built = [path.join(build, `extract-xiso${exe}`), path.join(build, "Release", `extract-xiso${exe}`)].find((p) =>
  fs.existsSync(p),
);
if (!built) {
  console.error("extract-xiso: build finished but no binary was found in " + build);
  process.exit(1);
}
fs.mkdirSync(outDir, { recursive: true });
fs.copyFileSync(built, out);
fs.chmodSync(out, 0o755);
console.log(`extract-xiso: ${path.relative(root, out)}`);
