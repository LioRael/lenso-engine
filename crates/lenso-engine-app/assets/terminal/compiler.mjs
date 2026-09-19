import fs from "node:fs";
import path from "node:path";
const request = JSON.parse(await Bun.stdin.text());
if (request.schema !== "lenso.convention-compile.v1") throw new Error("unsupported compiler request");
const output = request.output;
if (request.entry.endsWith(".rs")) {
  const sdk = path.join(import.meta.dir, "rust-sdk");
  const cargo = `[package]\nname = ${JSON.stringify(request.plugin_id.replaceAll(".", "-"))}\nversion = ${JSON.stringify(request.release_version)}\nedition = "2024"\n[workspace]\n[package.metadata.lenso]\nplugin-id = ${JSON.stringify(request.plugin_id)}\nroot-slot = "terminal-providers"\n[dependencies]\nlenso = "=0.5.23"\nanyhow = "1"\nlenso-cli-support = { path = ${JSON.stringify(sdk)} }\n`;
  fs.writeFileSync(path.join(output,"Cargo.toml"),cargo);
  fs.mkdirSync(path.join(output,"src"));
  fs.writeFileSync(path.join(output,"src/lib.rs"), `
#[path = ${JSON.stringify(request.entry)}] mod entry;
use lenso_cli_support as support;
#[lenso::plugin]
#[derive(Clone, Debug)]
struct Surface {}
#[lenso::provides(support::CommandProvider)]
impl support::CommandProviderProvider for Surface {
    fn catalog(&self, _context: support::Context, _request: support::CatalogRequest) -> support::CatalogFuture {
        support::catalog(entry::command(), ${JSON.stringify(request.plugin_id)})
    }
    fn execute(&self, _context: support::Context, request: support::ExecuteOpen) -> support::ExecuteFuture {
        support::execute(entry::command(), ${JSON.stringify(request.plugin_id)}, request)
    }
}
`);
  process.stdout.write(JSON.stringify({schema:"lenso.convention-compiled.v1"}));
  process.exit(0);
}
if (!request.entry.endsWith(".ts")) throw new Error("CLI conventions support .ts and .rs entries");
const root = import.meta.dir;
for (const name of ["provider.ts", "sdk.ts"]) fs.copyFileSync(path.join(root,name), path.join(output,name));
const name = path.basename(path.dirname(request.entry));
fs.writeFileSync(path.join(output,"plugin.ts"), [
  'import { definePlugin } from "@lenso/bun-plugin";',
  'import { CommandProvider } from "./provider.ts";',
  'import { createCommandProvider } from "./sdk.ts";',
  `import definition from ${JSON.stringify(request.entry)};`,
  `export default definePlugin({ provides: [CommandProvider], create() { return createCommandProvider(definition, ${JSON.stringify(request.plugin_id)}, ${JSON.stringify(name)}); } });`,
].join("\n"));
fs.writeFileSync(path.join(output,"package.json"), JSON.stringify({
  name: request.plugin_id, version: request.release_version, private: true, type: "module",
  dependencies: { "@lenso/bun-plugin":"0.4.1", "@lenso/contract-runtime":"0.3.0", "@lenso/cli":`file:${root}` },
  devDependencies: { typescript:"7.0.2", "@types/bun":"1.4.0" },
  scripts: { check:"tsc --noEmit" },
  lenso:{ pluginId:request.plugin_id, runtime:"bun", rootSlot:"terminal-providers", source:"plugin.ts" },
},null,2));
fs.writeFileSync(path.join(output,"tsconfig.json"), JSON.stringify({compilerOptions:{strict:true,noEmit:true,module:"Preserve",moduleResolution:"bundler",allowImportingTsExtensions:true,types:["bun"],paths:{"@lenso/cli":[path.join(root,"sdk.ts")]}},include:["*.ts"]}));
process.stdout.write(JSON.stringify({schema:"lenso.convention-compiled.v1"}));
