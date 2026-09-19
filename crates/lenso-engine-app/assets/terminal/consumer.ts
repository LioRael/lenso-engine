import { definePlugin } from "@lenso/bun-plugin";
import { Command } from "./command.ts";
export default definePlugin({ provides: [], dependencies: { commands: Command.required() }, create() { return {}; } });
