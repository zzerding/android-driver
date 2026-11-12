import { createInterface } from "node:readline";

export function createClient(child) {
  const lines = createInterface({ input: child.stdout });
  const pending = new Map();
  let nextId = 1;
  let started = false;
  let ending = false;
  let failure;
  let resolveReady;
  let rejectReady;
  const ready = new Promise((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });
  ready.catch(() => {});
  const closed = new Promise((resolve) => child.once("close", (code, signal) => resolve({ code, signal })));

  function fail(error) {
    if (failure) return;
    failure = error;
    rejectReady(error);
    for (const waiter of pending.values()) waiter.reject(error);
    pending.clear();
    child.stdin.destroy();
    child.kill();
  }

  child.on("error", fail);
  child.stdin.on("error", fail);
  child.stdout.on("error", fail);
  lines.on("error", fail);
  lines.on("close", () => {
    if (!ending || pending.size || !started) fail(new Error("driver stdout closed"));
  });
  child.on("close", (code, signal) => {
    if (code !== 0 || signal || pending.size || !started) {
      fail(new Error(`driver exited: code=${code}, signal=${signal}`));
    }
  });
  lines.on("line", (line) => {
    if (failure) return;
    try {
      const message = JSON.parse(line);
      if (!started) {
        if (message?.type !== "ready" || message.protocol !== 1) throw new Error("unsupported ready frame");
        started = true;
        resolveReady(message);
        return;
      }
      const waiter = pending.get(message?.id);
      if (message?.type !== "response" || !waiter || typeof message.ok !== "boolean"
          || (message.ok ? !("result" in message)
            : typeof message.error?.code !== "string" || typeof message.error?.message !== "string")) {
        throw new Error("invalid or duplicate driver response");
      }
      pending.delete(message.id);
      waiter.resolve(message);
    } catch (error) {
      fail(error);
    }
  });

  return {
    ready,
    async request(op, fields = {}) {
      await ready;
      if (failure) throw failure;
      if (ending) throw new Error("driver input already ended");
      const id = nextId++;
      const line = JSON.stringify({ ...fields, id, op }) + "\n";
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        child.stdin.write(line, (error) => {
          if (error) fail(error);
        });
      });
    },
    async end() {
      ending = true;
      child.stdin.end();
      await closed;
      if (failure) throw failure;
    },
  };
}
