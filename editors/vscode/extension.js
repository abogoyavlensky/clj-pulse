const path = require("path");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const serverPath = path.resolve(__dirname, "..", "..", "target", "debug", "clj-pulse");

  const serverOptions = {
    command: serverPath,
    transport: TransportKind.stdio,
  };

  const clientOptions = {
    documentSelector: [
      { scheme: "file", language: "clojure" },
    ],
  };

  client = new LanguageClient(
    "clj-pulse",
    "clj-pulse",
    serverOptions,
    clientOptions,
  );

  client.start();
  context.subscriptions.push(client);
}

function deactivate() {
  if (client) {
    return client.stop();
  }
}

module.exports = { activate, deactivate };
