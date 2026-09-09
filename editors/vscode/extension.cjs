const vscode = require('vscode');
const { LanguageClient } = require('vscode-languageclient/node');
let client;
exports.activate = async function activate(context) {
  const command = vscode.workspace.getConfiguration('ifx').get('serverPath', 'ifx-lang');
  client = new LanguageClient('ifx', 'IFX', { command, args: ['lsp'] }, {
    documentSelector: [{ scheme: 'file', language: 'ifx' }],
  });
  context.subscriptions.push(client);
  await client.start();
};
exports.deactivate = async function deactivate() {
  if (client) await client.stop();
};
