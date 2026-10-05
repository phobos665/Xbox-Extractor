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
// `tauri build --target universal-apple-darwin` wants one fat sidecar under this name.
if (triple === "universal-apple-darwin") configure.push("-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64");
if (triple.includes("apple-darwin")) configure.push("-DCMAKE_OSX_DEPLOYMENT_TARGET=11.0");
if (triple.includes("windows")) {
  if (process.platform === "win32") {
    // Visual Studio, as upstream extract-xiso's own CI builds it.
    configure.push("-A", triple.startsWith("aarch64") ? "ARM64" : "x64");
  } else {
    // Cross-building on macOS or Linux (to check a Windows build before CI): mingw-w64,
    // statically linked so the .exe needs no MinGW runtime DLLs.
    const cc = `${triple.startsWith("aarch64") ? "aarch64" : "x86_64"}-w64-mingw32-gcc`;
    configure.push(
      "-DCMAKE_SYSTEM_NAME=Windows",
      `-DCMAKE_C_COMPILER=${cc}`,
      "-DCMAKE_EXE_LINKER_FLAGS=-static",
    );
  }
}

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
// A universal build compiles each architecture on its own, and each half looks for a sidecar
// under its own triple as well as the universal one. The fat binary serves all three.
const names =
  triple === "universal-apple-darwin"
    ? [out, ...["aarch64-apple-darwin", "x86_64-apple-darwin"].map((t) => path.join(outDir, `extract-xiso-${t}`))]
    : [out];
for (const name of names) {
  fs.copyFileSync(built, name);
  fs.chmodSync(name, 0o755);
  console.log(`extract-xiso: ${path.relative(root, name)}`);
}
