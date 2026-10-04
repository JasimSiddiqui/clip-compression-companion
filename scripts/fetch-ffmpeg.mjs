// Puts the FFmpeg binary that ships with the app at
// src-tauri/binaries/ffmpeg-<target-triple>[.exe], where Tauri's externalBin expects it.
//
// Windows: downloads the "essentials" release build from gyan.dev (GPL, static,
// includes x264, x265, SVT-AV1, libwebp, LAME, Opus, and the NVENC/QSV/AMF encoders).
// macOS/Linux: copies the ffmpeg already on your PATH.
//
// Does nothing if the binary is already there. Run: npm run ffmpeg
import { execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ESSENTIALS_URL = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const binDir = join(root, "src-tauri", "binaries");
const triple = execFileSync("rustc", ["-vV"], { encoding: "utf8" }).match(/^host: (\S+)/m)[1];
const isWindows = process.platform === "win32";
const target = join(binDir, `ffmpeg-${triple}${isWindows ? ".exe" : ""}`);

if (existsSync(target)) process.exit(0);
mkdirSync(binDir, { recursive: true });

if (!isWindows) {
  const which = execFileSync("sh", ["-c", "command -v ffmpeg"], { encoding: "utf8" }).trim();
  if (!which) throw new Error("ffmpeg not found on PATH. Install it with your package manager first.");
  copyFileSync(which, target);
  console.log(`copied ${which} -> ${target}`);
  process.exit(0);
}

const work = mkdtempSync(join(tmpdir(), "ccc-ffmpeg-"));
try {
  console.log(`downloading ${ESSENTIALS_URL}`);
  const res = await fetch(ESSENTIALS_URL);
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status}`);
  const zip = join(work, "ffmpeg.zip");
  writeFileSync(zip, Buffer.from(await res.arrayBuffer()));

  // Windows 10+ ships bsdtar, which reads .zip files. Call it by full path so a
  // GNU tar from Git Bash or MSYS2 on the PATH isn't picked up instead.
  const tar = join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe");
  execFileSync(tar, ["-xf", zip, "-C", work]);
  const folder = readdirSync(work).find((name) => name.startsWith("ffmpeg-") && statSync(join(work, name)).isDirectory());
  if (!folder) throw new Error("unexpected archive layout");
  copyFileSync(join(work, folder, "bin", "ffmpeg.exe"), target);
  console.log(`${folder} -> ${target}`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
