// Runs inside the VS Code extension host: Go to Definition end to end, through
// VS Code's own command, the real rq binary, and the Ruby fixture.
import * as assert from "node:assert/strict";
import * as path from "node:path";
import * as vscode from "vscode";

type Def = vscode.Location | vscode.LocationLink;

async function definitions(doc: vscode.TextDocument, needle: string, offset: number): Promise<string[]> {
  const line = doc.getText().split("\n").findIndex((l) => l.includes(needle));
  assert.ok(line >= 0, `fixture has no ${needle}`);
  const col = doc.lineAt(line).text.indexOf(needle) + offset;
  const defs = await vscode.commands.executeCommand<Def[]>(
    "vscode.executeDefinitionProvider",
    doc.uri,
    new vscode.Position(line, col),
  );
  const root = vscode.workspace.workspaceFolders![0].uri.fsPath;
  return defs.map((d) => {
    const [uri, range] = "targetUri" in d ? [d.targetUri, d.targetRange] : [d.uri, d.range];
    return `${path.relative(root, uri.fsPath)}:${range.start.line + 1}`;
  });
}

export async function run() {
  const root = vscode.workspace.workspaceFolders![0].uri;
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.joinPath(root, "app.rb"));
  await vscode.window.showTextDocument(doc);
  await vscode.extensions.getExtension("dpep.rq")!.activate();

  // Widget.new → the initializer
  assert.deepEqual(await definitions(doc, "Widget.new", "Widget.".length + 1), ["lib/widget.rb:4"]);
  // Account::Ledger → the nested class
  assert.deepEqual(await definitions(doc, "Account::Ledger", "Account::".length + 1), ["lib/account.rb:2"]);
  // Account::Ledger.open → the class method in that scope
  assert.deepEqual(await definitions(doc, "Ledger.open", "Ledger.".length + 1), ["lib/account.rb:3"]);
  // widget.empty? → the predicate, suffix included
  assert.deepEqual(await definitions(doc, "widget.empty?", "widget.".length + 1), ["lib/widget.rb:12"]);
  // widget.save → a variable's class is unknown, so both saves (a peek list)
  assert.deepEqual((await definitions(doc, "widget.save", "widget.".length + 1)).sort(), [
    "lib/account.rb:7",
    "lib/widget.rb:8",
  ]);
  // Ledger.new has no initializer of its own → the class
  assert.deepEqual(await definitions(doc, "Ledger.new", "Ledger.".length + 1), ["lib/account.rb:2"]);
  // A name rq doesn't know → nothing, not a fuzzy neighbour
  assert.deepEqual(await definitions(doc, "wid.sav", "wid.".length + 1), []);

  // Cmd-T: rq's ranked hits, as workspace symbols
  const symbols = await vscode.commands.executeCommand<vscode.SymbolInformation[]>(
    "vscode.executeWorkspaceSymbolProvider",
    "Ledger",
  );
  const ledger = symbols.find((s) => s.name === "Ledger");
  assert.ok(ledger, `no Ledger in ${symbols.map((s) => s.name)}`);
  assert.equal(ledger.kind, vscode.SymbolKind.Class);
  assert.equal(ledger.containerName, "Account");
}
