// Placeholder for Task 3: only check 1 (extension installed and active).
const vscode = require("vscode");

exports.run = async () => {
  const ext = vscode.extensions.getExtension("abogoyavlensky.clojure-pulse");
  if (!ext) {
    throw new Error("Clojure Pulse extension not installed");
  }
  if (!ext.isActive) {
    await ext.activate();
  }
  console.log(`ok    Clojure Pulse extension active (${ext.packageJSON.version})`);
};
