import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { createClient } from "./node-client.mjs";

function peer(source) {
  return spawn(process.execPath, ["--input-type=module", "-e", source], {
    stdio: ["pipe", "pipe", "pipe"],
  });
}

const ready = 'console.log(JSON.stringify({type:"ready",protocol:1}));';

test("JSON-lines requests preserve IDs and structured error results", { timeout: 5000 }, async () => {
  const child = peer(`${ready}
    let last = 0;
    const {createInterface} = await import('node:readline');
    createInterface({input:process.stdin}).on('line', line => {
      const request = JSON.parse(line);
      if (request.id !== ++last || !['status','launch'].includes(request.op)) process.exit(2);
      console.log(JSON.stringify(request.op === 'status'
        ? {type:'response',id:request.id,ok:true,result:{state:'idle'}}
        : {type:'response',id:request.id,ok:false,error:{code:'Timeout',message:'unknown',details:{result_unknown:true}}}));
    });`);
  const client = createClient(child);
  try {
    const first = await client.request("status", { id: 99, op: "open" });
    assert.equal(first.id, 1);
    assert.equal(first.result.state, "idle");
    const second = await client.request("launch", { component: "com.example/.Main" });
    assert.equal(second.id, 2);
    assert.equal(second.error.details.result_unknown, true);
  } finally {
    await client.end();
  }
});

for (const [name, source] of [
  ["invalid ready version", 'console.log(JSON.stringify({type:"ready",protocol:99}));'],
  ["malformed output", `${ready} console.log('not json');`],
  ["unknown response ID", `${ready} console.log(JSON.stringify({type:'response',id:99,ok:true,result:{}}));`],
  ["stdout EOF", `${ready} process.stdout.end();`],
  ["abnormal exit", `${ready} process.exitCode=7;`],
]) {
  test(`${name} rejects all pending requests`, { timeout: 5000 }, async () => {
    const child = peer(source);
    const client = createClient(child);
    try {
      const outcomes = await Promise.allSettled([client.request("status"), client.request("status")]);
      assert.ok(outcomes.every((outcome) => outcome.status === "rejected"));
      await assert.rejects(client.request("status"));
    } finally {
      await client.end().catch(() => {});
    }
  });
}

test("spawn errors reject ready and requests", { timeout: 5000 }, async () => {
  const child = spawn("/nonexistent/android-driver-example", [], { stdio: ["pipe", "pipe", "pipe"] });
  const client = createClient(child);
  await assert.rejects(client.request("status"));
  await assert.rejects(client.end());
});

test("stdin failure rejects pending requests", { timeout: 5000 }, async () => {
  const child = peer(`${ready} process.stdin.resume();`);
  const client = createClient(child);
  await client.ready;
  const outcomes = Promise.allSettled([client.request("status"), client.request("status")]);
  child.stdin.destroy(new Error("injected pipe failure"));
  assert.ok((await outcomes).every((outcome) => outcome.status === "rejected"));
  await assert.rejects(client.end());
});

test("README snippets match the checked standalone examples", async () => {
  const root = new URL("../", import.meta.url);
  const readme = await readFile(new URL("README.md", root), "utf8");
  for (const [fence, path] of [
    ["rust,no_run", "crates/android-driver/examples/session.rs"],
    ["js", "examples/node.mjs"],
  ]) {
    const code = await readFile(new URL(path, root), "utf8");
    assert.equal(readme.split(`\`\`\`${fence}\n`)[1]?.split("```", 1)[0], code);
  }
});
