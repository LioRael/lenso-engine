import type { CommandDefinition, CommandProviderProvider, ExecuteError, ExecuteMessage, InvocationContext, StreamEvent, StreamSession } from "./provider.ts";
export type Argument = { type?: "string" | "boolean"; description?: string; default?: string | boolean; required?: boolean };
export type Command = {
  name?: string;
  description: string;
  args?: Record<string, Argument>;
  run(input: { args: Record<string, string | boolean>; context: InvocationContext; output: { text(value: string): void; error(value: string): void } }): void | Promise<void>;
};
export function command<T extends Command>(value: T): T { return value; }

/** Uses the terminal-owned generated Provider contract, never a second command wire protocol. */
export function createCommandProvider(definition: Command, id: string, defaultName: string): CommandProviderProvider {
  const parameters = Object.entries(definition.args ?? {}).map(([id, arg]) => ({
    id, kind: arg.type === "boolean" ? "flag" as const : "option" as const,
    long: id, description: arg.description ?? "", required: arg.required ?? false, multiple: false, choices: [],
  }));
  const catalog: CommandDefinition = { id, path: (definition.name ?? defaultName).split(" "), summary: definition.description,
    description: definition.description, parameters, output_formats: ["text"] };
  return {
    async catalog() { return { ok: true, value: { commands: [catalog] } }; },
    async execute(context, request) {
      if (request.id !== id) return { ok: false, error: { kind: "domain", error: "not_found" } };
      let args: Record<string, string | boolean>;
      try {
        const parsed = JSON.parse(request.arguments_json);
        if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("object required");
        args = Object.create(null);
        for (const key of Object.keys(parsed)) if (!Object.hasOwn(definition.args ?? {}, key)) throw new Error("unknown argument");
        for (const [key, rule] of Object.entries(definition.args ?? {})) {
          const value = parsed[key] ?? rule.default ?? (rule.type === "boolean" ? false : undefined);
          if (value === undefined) { if (rule.required) throw new Error("required argument"); continue; }
          if (typeof value !== (rule.type === "boolean" ? "boolean" : "string")) throw new Error("argument type");
          args[key] = value;
        }
      } catch { return { ok: false, error: { kind: "domain", error: "invalid_arguments" } }; }
      let queue: ExecuteMessage[] = [], bytes = 0, cancelled = false, finished = false, terminalRead = false;
      let terminal: StreamEvent<ExecuteMessage, ExecuteError> = { kind: "terminal", outcome: { ok: true } };
      let wake: (() => void) | undefined;
      const output = (kind: "stdout" | "stderr", content: string) => {
        if (cancelled) throw new Error("command cancelled");
        bytes += new TextEncoder().encode(content).length;
        if (queue.length >= 256 || bytes > 16*1024*1024) throw new Error("output_limit_exceeded");
        queue.push({ kind, content_type: "text", content }); wake?.(); wake = undefined;
      };
      void Promise.resolve().then(() => definition.run({ args, context, output: {
        text: value => output("stdout", `${value}\n`), error: value => output("stderr", `${value}\n`),
      } })).catch(error => {
        terminal = { kind: "terminal", outcome: { ok: false, error: error?.message === "output_limit_exceeded" ? "output_limit_exceeded" : {
          code: "execution_failed", payload: { reason_code: "command_failed", message: String(error?.message ?? error).slice(0,4096), details_json: "{}" },
        } } };
      }).finally(() => { finished = true; wake?.(); wake = undefined; });
      const stream: StreamSession<ExecuteMessage, ExecuteError> = {
        async send() { throw new Error("CLI commands do not accept stream input"); },
        async closeSend() {},
        cancel() { cancelled = true; queue = []; finished = true; wake?.(); wake = undefined; },
        async receive() {
          while (!queue.length && !finished) await new Promise<void>(resolve => { wake = resolve; });
          if (terminalRead || cancelled) throw new Error("stream closed");
          const message = queue.shift();
          if (message) return { kind: "message", message };
          terminalRead = true; return terminal;
        },
      };
      return { ok: true, value: stream };
    },
  };
}
