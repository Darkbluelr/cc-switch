import { spawn } from "node:child_process";

const LOCAL_CONFIG_PATH = "src-tauri/tauri.local.conf.json";
const PNPM_BIN = process.platform === "win32" ? "pnpm.cmd" : "pnpm";

function hasConfigFlag(args) {
  return args.includes("--config") || args.includes("-c");
}

function insertConfig(args, afterIndex) {
  const dashDashIndex = args.indexOf("--");
  const insertAt =
    dashDashIndex === -1 ? afterIndex + 1 : Math.min(afterIndex + 1, dashDashIndex);
  args.splice(insertAt, 0, "--config", LOCAL_CONFIG_PATH);
}

function maybeInjectLocalConfig(args) {
  if (args.length === 0 || hasConfigFlag(args)) return;

  const cmd = args[0];
  if (cmd === "build" || cmd === "dev" || cmd === "bundle") {
    insertConfig(args, 0);
    return;
  }

  // Mobile subcommands: `tauri android build|dev ...` / `tauri ios build|dev ...`
  if ((cmd === "android" || cmd === "ios") && args.length >= 2) {
    const sub = args[1];
    if (sub === "build" || sub === "dev" || sub === "bundle") {
      insertConfig(args, 1);
    }
  }
}

const tauriArgs = process.argv.slice(2);
maybeInjectLocalConfig(tauriArgs);

const child = spawn(PNPM_BIN, ["exec", "tauri", ...tauriArgs], {
  stdio: "inherit",
});

child.on("exit", (code) => {
  process.exit(code ?? 1);
});

child.on("error", (err) => {
  // eslint-disable-next-line no-console
  console.error(err);
  process.exit(1);
});
