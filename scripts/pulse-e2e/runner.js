// Headless e2e against the first-priority editor setup: downloads VS Code,
// installs the real Clojure Pulse extension, points clojurePulse.server.path at
// our binary, and runs the checks inside the extension host (tests.js).
//
// The .vsix comes from, in order: $CLJ_PULSE_VSIX, the sibling
// ../clojure-pulse-vscode checkout (packaged with vsce), or the latest GitHub
// release. Set CLJ_PULSE_E2E_NO_LOCAL=1 to skip the sibling checkout and
// exercise the release path.
//
// Usage: xvfb-run -a node runner.js

const path = require("path");
const fs = require("fs");
const os = require("os");
const cp = require("child_process");
const {
  downloadAndUnzipVSCode,
  resolveCliArgsFromVSCodeExecutablePath,
  runTests,
} = require("@vscode/test-electron");

const EXT_REPO = "abogoyavlensky/clojure-pulse-vscode";
const EXT_CHECKOUT = path.resolve(__dirname, "../../../clojure-pulse-vscode");
const TEST_DIR = path.join(__dirname, ".vscode-test");

/** Captured stdout of a command run in `cwd`, trimmed. */
function capture(cmd, cwd) {
  return cp.execSync(cmd, { cwd, encoding: "utf-8" }).trim();
}

/**
 * Packages the sibling checkout, reusing the last .vsix when HEAD is unchanged
 * and its tree is clean — a dirty tree is repackaged every run so local
 * extension edits are actually under test, and under its own name so reverting
 * those edits never leaves a dirty build sitting at the clean-HEAD path.
 */
function packageLocalExtension() {
  const pkg = JSON.parse(fs.readFileSync(path.join(EXT_CHECKOUT, "package.json"), "utf-8"));
  const sha = capture("git rev-parse --short HEAD", EXT_CHECKOUT);
  const dirty = capture("git status --porcelain", EXT_CHECKOUT) !== "";
  const name = `clojure-pulse-${pkg.version}-${sha}${dirty ? "-dirty" : ""}.vsix`;
  const vsix = path.join(TEST_DIR, name);

  if (fs.existsSync(vsix) && !dirty) {
    return vsix;
  }
  if (!fs.existsSync(path.join(EXT_CHECKOUT, "node_modules"))) {
    console.log("installing extension dependencies (npm ci)…");
    cp.execSync("npm ci", { cwd: EXT_CHECKOUT, stdio: "inherit" });
  }
  console.log(`packaging ${EXT_CHECKOUT} (${sha}${dirty ? ", dirty" : ""})…`);
  cp.execSync(`npx vsce package -o ${vsix}`, { cwd: EXT_CHECKOUT, stdio: "inherit" });
  return vsix;
}

/** Downloads the latest release .vsix once. */
function downloadReleaseExtension() {
  const vsix = path.join(TEST_DIR, "clojure-pulse-release.vsix");
  if (fs.existsSync(vsix)) {
    return vsix;
  }
  const release = JSON.parse(
    capture(`curl -sL https://api.github.com/repos/${EXT_REPO}/releases/latest`, __dirname)
  );
  const asset = (release.assets ?? []).find((a) => a.name.endsWith(".vsix"));
  if (!asset) {
    throw new Error(`latest release of ${EXT_REPO} has no .vsix asset`);
  }
  console.log(`downloading ${asset.name}…`);
  cp.execSync(`curl -sL -o ${vsix} ${asset.browser_download_url}`);
  return vsix;
}

function resolveVsix() {
  if (process.env.CLJ_PULSE_VSIX) {
    const vsix = path.resolve(process.env.CLJ_PULSE_VSIX);
    if (!fs.existsSync(vsix)) {
      throw new Error(`CLJ_PULSE_VSIX does not exist: ${vsix}`);
    }
    console.log(`extension: $CLJ_PULSE_VSIX (${vsix})`);
    return vsix;
  }
  if (!process.env.CLJ_PULSE_E2E_NO_LOCAL && fs.existsSync(path.join(EXT_CHECKOUT, "package.json"))) {
    console.log(`extension: sibling checkout ${EXT_CHECKOUT}`);
    return packageLocalExtension();
  }
  console.log(`extension: latest GitHub release of ${EXT_REPO}`);
  return downloadReleaseExtension();
}

async function main() {
  const serverBin = path.resolve(__dirname, "../../target/debug/clj-pulse");
  if (!fs.existsSync(serverBin)) {
    throw new Error(`server binary not found, run cargo build first: ${serverBin}`);
  }
  fs.mkdirSync(TEST_DIR, { recursive: true });

  // 1. Copy the fixture project to a temp dir and generate a real .cpcache
  const work = fs.mkdtempSync(path.join(os.tmpdir(), "pulse-e2e-"));
  fs.cpSync(path.join(__dirname, "fixture"), work, { recursive: true });
  console.log(`fixture project: ${work}`);
  cp.execSync("clojure -Spath", { cwd: work, stdio: ["ignore", "ignore", "inherit"] });

  // 2. Workspace settings: use our server
  fs.mkdirSync(path.join(work, ".vscode"), { recursive: true });
  fs.writeFileSync(
    path.join(work, ".vscode", "settings.json"),
    JSON.stringify({ "clojurePulse.server.path": serverBin })
  );

  // 3. VS Code (shared download cache) + the Clojure Pulse extension
  const vscodeExecutablePath = await downloadAndUnzipVSCode({
    version: "stable",
    cachePath: path.resolve(__dirname, "../.vscode-cache"),
  });
  const extensionsDir = path.join(TEST_DIR, "extensions");
  const userDataDir = path.join(TEST_DIR, "user-data");
  // The resolver injects its own --extensions-dir/--user-data-dir under the
  // default cache path; drop them so ours are the only ones VS Code sees.
  const [cliPath, ...cliArgs] = resolveCliArgsFromVSCodeExecutablePath(vscodeExecutablePath).filter(
    (a) => !a.startsWith("--extensions-dir") && !a.startsWith("--user-data-dir")
  );

  // --force so an older .vsix (the release fallback, an explicit
  // $CLJ_PULSE_VSIX) replaces a newer one left in the persistent extensions
  // dir; without it VS Code refuses the downgrade and the tests would silently
  // run against the extension from a previous run.
  const vsix = resolveVsix();
  const install = cp.spawnSync(
    cliPath,
    [
      ...cliArgs,
      "--extensions-dir",
      extensionsDir,
      "--user-data-dir",
      userDataDir,
      "--install-extension",
      vsix,
      "--force",
    ],
    { encoding: "utf-8", stdio: "inherit" }
  );
  if (install.error) {
    throw install.error;
  }
  if (install.status !== 0) {
    throw new Error(`installing ${path.basename(vsix)} failed with exit code ${install.status}`);
  }

  // 4. Run the checks inside the extension host
  await runTests({
    vscodeExecutablePath,
    extensionDevelopmentPath: path.join(__dirname, "test-ext"),
    extensionTestsPath: path.join(__dirname, "tests.js"),
    launchArgs: [
      work,
      "--extensions-dir",
      extensionsDir,
      "--user-data-dir",
      userDataDir,
      "--disable-workspace-trust",
      "--disable-gpu",
      "--no-sandbox",
    ],
  });
  console.log("PULSE E2E PASSED");
}

main().catch((e) => {
  console.error("PULSE E2E FAILED:", e.message ?? e);
  process.exit(1);
});
