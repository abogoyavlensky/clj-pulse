const path = require("path");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const serverPath = path.resolve(__dirname, "..", "..", "target", "debug", "clj-lsp");

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
    "clj-lsp",
    "clj-lsp",
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
