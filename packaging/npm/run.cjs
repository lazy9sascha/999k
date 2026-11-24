const { spawn } = require("node:child_process");
const { join } = require("node:path");

module.exports = (binary) => {
  const child = spawn("sh", [join(__dirname, "install.sh"), "--run", binary, ...process.argv.slice(2)], {
    stdio: "inherit",
  });
  const signals = ["SIGINT", "SIGTERM", "SIGHUP"];
  const handlers = signals.map((signal) => {
    const handler = () => child.kill(signal);
    process.on(signal, handler);
    return handler;
  });
  child.on("error", (error) => {
    console.error(`999k: Cannot start the installer: ${error.message}`);
    process.exitCode = 1;
  });
  child.on("close", (code, signal) => {
    signals.forEach((name, index) => process.removeListener(name, handlers[index]));
    if (signal) process.kill(process.pid, signal);
    else process.exitCode = code ?? 1;
  });
};
