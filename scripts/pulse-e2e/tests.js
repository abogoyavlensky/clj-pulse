// Runs inside the VS Code extension host with the real Clojure Pulse extension
// activated. Exercises the user-visible surface of the server through the
// extension's own client: project and library definition, jar: content served
// by the extension's clojure/dependencyContents provider, hover, completion,
// and diagnostics.

const vscode = require("vscode");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function positionOf(doc, needle, offset) {
  const lines = doc.getText().split("\n");
  for (let i = 0; i < lines.length; i++) {
    const col = lines[i].indexOf(needle);
    if (col >= 0) {
      return new vscode.Position(i, col + (offset ?? Math.floor(needle.length / 2)));
    }
  }
  throw new Error(`${needle} not found in ${doc.uri}`);
}

async function poll(timeoutMs, fn) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await fn();
    if (last !== undefined) {
      return last;
    }
    await sleep(500);
  }
  return last;
}

async function definitionsAt(uri, pos, timeoutMs) {
  const locs = await poll(timeoutMs, async () => {
    const r = await vscode.commands.executeCommand("vscode.executeDefinitionProvider", uri, pos);
    return r && r.length > 0 ? r : undefined;
  });
  return locs ?? [];
}

/** Hover contents flattened to plain strings. */
async function hoverAt(uri, pos, timeoutMs) {
  const texts = await poll(timeoutMs, async () => {
    const hovers = await vscode.commands.executeCommand("vscode.executeHoverProvider", uri, pos);
    const out = (hovers ?? []).flatMap((h) =>
      (h.contents ?? []).map((c) => (typeof c === "string" ? c : (c.value ?? "")))
    );
    return out.length > 0 ? out : undefined;
  });
  return texts ?? [];
}

async function completionsAt(uri, pos, timeoutMs) {
  const items = await poll(timeoutMs, async () => {
    const list = await vscode.commands.executeCommand(
      "vscode.executeCompletionItemProvider",
      uri,
      pos
    );
    return list && list.items.length > 0 ? list.items : undefined;
  });
  return items ?? [];
}

async function diagnosticsAt(uri, timeoutMs, predicate) {
  const diags = await poll(timeoutMs, async () => {
    const all = vscode.languages.getDiagnostics(uri);
    return all.some(predicate) ? all : undefined;
  });
  return diags ?? [];
}

/** The code arrives as a plain string or as `{ value, target }`. */
const codeOf = (d) => (typeof d.code === "object" ? d.code?.value : d.code);

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

  // 1. The extension itself
  const ext = vscode.extensions.getExtension("abogoyavlensky.clojure-pulse");
  check(ext !== undefined, "Clojure Pulse extension installed");
  if (ext && !ext.isActive) {
    await ext.activate().catch((e) => check(false, "Clojure Pulse activation", e.message));
  }
  check(ext?.isActive === true, "Clojure Pulse activated");

  // 2. Project-internal navigation (also the wait for the first index pass)
  const helperPos = positionOf(doc, "o/helper");
  const projLocs = await definitionsAt(appUri, helperPos, 60000);
  const projUri = projLocs[0]?.uri ?? projLocs[0]?.targetUri;
  check(
    projLocs.length > 0 && projUri?.path?.endsWith("/src/other.clj"),
    "project definition: o/helper -> src/other.clj",
    JSON.stringify(projLocs.map((l) => (l.uri ?? l.targetUri)?.toString()))
  );

  // 3. Library navigation and jar: content through the extension's own
  //    clojure/dependencyContents provider
  const libLocs = await definitionsAt(appUri, positionOf(doc, "json/write-str"), 60000);
  const libUri = libLocs[0]?.uri ?? libLocs[0]?.targetUri;
  check(
    libLocs.length > 0 && libUri?.scheme === "jar",
    "library definition: json/write-str returns a jar: location",
    libUri?.toString() ?? "definition provider returned nothing"
  );
  if (libUri) {
    try {
      const jarDoc = await vscode.workspace.openTextDocument(libUri);
      const text = jarDoc.getText();
      check(
        text.includes("(defn write-str"),
        "jar: document opens with library source via clojure/dependencyContents",
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

  // 4. Hover shows the docstring
  const hovers = await hoverAt(appUri, helperPos, 30000);
  check(
    hovers.some((h) => h.includes("A project-internal helper.")),
    "hover: o/helper shows its docstring",
    JSON.stringify(hovers)
  );

  // 5. Completion of the alias-qualified name, requested from inside `o/he`
  const items = await completionsAt(appUri, positionOf(doc, "o/helper", 4), 30000);
  const labels = items.map((i) => (typeof i.label === "string" ? i.label : i.label?.label));
  check(
    labels.some((l) => l?.includes("helper")),
    "completion: o/he offers o/helper",
    JSON.stringify(labels.slice(0, 20))
  );

  // 6. Diagnostics: lint.clj requires clojure.set and never uses it. The code
  //    is the assertion — the source is clj-kondo when it is installed and
  //    clj-pulse when it is not, and both are correct.
  const lintUri = vscode.Uri.file(`${root}/src/lint.clj`);
  await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(lintUri));
  const diags = await diagnosticsAt(lintUri, 60000, (d) => codeOf(d) === "unused-namespace");
  check(
    diags.some((d) => codeOf(d) === "unused-namespace"),
    "diagnostics: unused clojure.set require is reported as unused-namespace",
    JSON.stringify(diags.map((d) => ({ code: codeOf(d), source: d.source, message: d.message })))
  );

  const failed = checks.filter((c) => !c.cond);
  if (failed.length > 0) {
    throw new Error(`${failed.length} check(s) failed: ${failed.map((c) => c.msg).join("; ")}`);
  }
};
