import { spawn } from "node:child_process";
import { createClient } from "./node-client.mjs";

const child = spawn("android-driver", ["--helper", "helper/build/virtual-display-helper.zip"], {
  stdio: ["pipe", "pipe", "inherit"],
});
const client = createClient(child);

function checked(response) {
  if (!response.ok) {
    throw Object.assign(new Error(response.error.message), { driverError: response.error });
  }
  return response.result;
}

try {
  checked(await client.request("open", { serial: "DEVICE_SERIAL", user: 0 }));
  checked(await client.request("launch", { component: "com.example.app/.Main" }));
  const screenshot = checked(await client.request("screenshot"));
  console.log(`captured ${screenshot.width}x${screenshot.height}`);
  checked(await client.request("close"));
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  try {
    await client.end();
  } catch (error) {
    console.error("driver shutdown:", error);
    process.exitCode = 1;
  }
}
