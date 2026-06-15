// Headless e2e against the user's real setup: downloads VS Code, installs
// the real Calva extension, points calva.clojureLspPath at our binary, and
// runs definition/jar-navigation checks inside the extension host (tests.js).
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

const CALVA_VSIX_URL =
  "https://open-vsx.org/api/betterthantomorrow/calva/2.0.591/file/betterthantomorrow.calva-2.0.591.vsix";

async function main() {
  const serverBin = path.resolve(__dirname, "../../target/debug/clj-pulse");
  if (!fs.existsSync(serverBin)) {
    throw new Error(`server binary not found, run cargo build first: ${serverBin}`);
  }

  // 1. Copy the fixture project to a temp dir and generate a real .cpcache
  const work = fs.mkdtempSync(path.join(os.tmpdir(), "calva-e2e-"));
  fs.cpSync(path.join(__dirname, "fixture"), work, { recursive: true });
  console.log(`fixture project: ${work}`);
  cp.execSync("clojure -Spath", { cwd: work, stdio: ["ignore", "ignore", "inherit"] });

  // 2. Workspace settings: use our server, LSP-first definitions
  fs.mkdirSync(path.join(work, ".vscode"), { recursive: true });
  fs.writeFileSync(
    path.join(work, ".vscode", "settings.json"),
    JSON.stringify({
      "calva.clojureLspPath": serverBin,
      "calva.definitionProviderPriority": ["lsp", "repl"],
      "calva.showCalvaSaysOnStart": false,
    })
  );

  // 3. VS Code + Calva (shared extensions dir so the test run sees it)
  const vscodeExecutablePath = await downloadAndUnzipVSCode("stable");
  const extensionsDir = path.join(__dirname, ".vscode-test", "extensions");
  const [cliPath, ...cliArgs] = resolveCliArgsFromVSCodeExecutablePath(vscodeExecutablePath);

  const vsix = path.join(__dirname, ".vscode-test", "calva.vsix");
  if (!fs.existsSync(vsix)) {
    console.log("downloading Calva vsix…");
    cp.execSync(`curl -sL -o ${vsix} ${CALVA_VSIX_URL}`);
  }
  cp.spawnSync(cliPath, [...cliArgs, "--extensions-dir", extensionsDir, "--install-extension", vsix], {
    encoding: "utf-8",
    stdio: "inherit",
  });

  // 4. Run the checks inside the extension host
  await runTests({
    vscodeExecutablePath,
    extensionDevelopmentPath: path.join(__dirname, "test-ext"),
    extensionTestsPath: path.join(__dirname, "tests.js"),
    launchArgs: [
      work,
      "--extensions-dir",
      extensionsDir,
      "--disable-workspace-trust",
      "--disable-gpu",
      "--no-sandbox",
    ],
  });
  console.log("CALVA E2E PASSED");
}

main().catch((e) => {
  console.error("CALVA E2E FAILED:", e.message ?? e);
  process.exit(1);
});
