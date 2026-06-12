// Runs inside the VS Code extension host with real Calva activated.
// Exercises the exact user flow: open a Clojure file, go-to-definition on a
// project symbol (baseline) and on a library symbol (jar: navigation), then
// open the jar: document the way the editor would.

const vscode = require("vscode");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function positionOf(doc, needle) {
  const text = doc.getText();
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const col = lines[i].indexOf(needle);
    if (col >= 0) {
      return new vscode.Position(i, col + Math.floor(needle.length / 2));
    }
  }
  throw new Error(`${needle} not found in ${doc.uri}`);
}

async function definitionsAt(uri, pos, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let locs = [];
  while (Date.now() < deadline) {
    locs = await vscode.commands.executeCommand("vscode.executeDefinitionProvider", uri, pos);
    if (locs && locs.length > 0) {
      return locs;
    }
    await sleep(500);
  }
  return locs ?? [];
}

exports.run = async () => {
  const checks = [];
  const check = (cond, msg, extra) => {
    checks.push({ cond, msg });
    console.log(`${cond ? "ok   " : "FAIL "} ${msg}${cond || extra === undefined ? "" : ` — ${extra}`}`);
  };

  const root = vscode.workspace.workspaceFolders[0].uri.fsPath;
  const appUri = vscode.Uri.file(`${root}/src/app.clj`);
  const doc = await vscode.workspace.openTextDocument(appUri);
  await vscode.window.showTextDocument(doc);

  const calva = vscode.extensions.getExtension("betterthantomorrow.calva");
  check(calva !== undefined, "Calva extension installed");
  if (calva && !calva.isActive) {
    await calva.activate().catch((e) => check(false, "Calva activation", e.message));
  }
  check(calva?.isActive === true, "Calva activated");

  // Baseline: project-internal navigation (waits for indexing on first hit)
  const projLocs = await definitionsAt(appUri, positionOf(doc, "o/helper"), 60000);
  const projUri = projLocs[0]?.uri ?? projLocs[0]?.targetUri;
  check(
    projLocs.length > 0 && projUri?.path?.endsWith("/src/other.clj"),
    "project definition: o/helper -> src/other.clj",
    JSON.stringify(projLocs.map((l) => (l.uri ?? l.targetUri)?.toString()))
  );

  // Library navigation: should produce a jar: URI through Calva's pipeline
  const libLocs = await definitionsAt(appUri, positionOf(doc, "json/write-str"), 60000);
  const libUri = libLocs[0]?.uri ?? libLocs[0]?.targetUri;
  check(
    libLocs.length > 0,
    "library definition: json/write-str returns a location",
    "definition provider returned nothing"
  );
  check(
    libUri?.scheme === "jar",
    "library definition has jar: scheme",
    libUri?.toString()
  );

  // Open the jar: document exactly like the editor does on click
  if (libUri) {
    try {
      const jarDoc = await vscode.workspace.openTextDocument(libUri);
      const text = jarDoc.getText();
      check(
        text.includes("(defn write-str"),
        "jar: document opens with library source via Calva's content provider",
        `got ${text.length} chars: ${JSON.stringify(text.slice(0, 120))}`
      );
      const line = libLocs[0].range?.start?.line ?? libLocs[0].targetRange?.start?.line;
      check(
        typeof line === "number" && jarDoc.lineAt(line).text.includes("write-str"),
        "definition range points at write-str within the jar source",
        `line ${line}: ${typeof line === "number" ? JSON.stringify(jarDoc.lineAt(line).text) : "n/a"}`
      );
    } catch (e) {
      check(false, "opening jar: document", e.message);
    }
  }

  const failed = checks.filter((c) => !c.cond);
  if (failed.length > 0) {
    throw new Error(`${failed.length} check(s) failed: ${failed.map((c) => c.msg).join("; ")}`);
  }
};
