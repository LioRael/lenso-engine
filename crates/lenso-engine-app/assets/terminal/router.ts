import { definePlugin } from "@lenso/bun-plugin";
import { Command, type CommandProvider as RouterProvider } from "./command.ts";
import { CommandProvider } from "./provider.ts";
export default definePlugin({
  provides: [Command], dependencies: { providers: CommandProvider.many() },
  create({ dependencies }): RouterProvider {
    async function routes(context: Parameters<RouterProvider["catalog"]>[0]) {
      const commands = [], byId = new Map(), paths = new Set<string>();
      for (const provider of [...dependencies.providers].sort((a,b) => a.providerInstance.localeCompare(b.providerInstance))) {
        const result = await provider.client.catalog({}, context);
        if (!result.ok) throw new Error("provider catalog unavailable");
        for (const command of result.value.commands) {
          const path = command.path.join(" ");
          if (!command.id || !path || paths.has(path) || byId.has(command.id) || commands.length >= 256) throw new Error("conflicting terminal catalog");
          paths.add(path); byId.set(command.id, provider.client); commands.push(command);
        }
      }
      return { commands, byId };
    }
    return {
      async catalog(context) {
        try { const { commands } = await routes(context); return { ok: true, value: { commands } }; }
        catch { return { ok: false, error: { kind: "domain", error: "catalog_invalid" } }; }
      },
      async execute(context, request) {
        let route;
        try { route = (await routes(context)).byId.get(request.id); }
        catch { return { ok: false, error: { kind: "domain", error: "not_found" } }; }
        if (!route) return { ok: false, error: { kind: "domain", error: "not_found" } };
        return route.execute(request, context);
      },
    };
  },
});
