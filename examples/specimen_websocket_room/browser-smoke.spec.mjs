import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createInterface } from "node:readline";

async function startServer() {
  const child = spawn(
    "cargo",
    ["run", "--quiet", "--manifest-path", "examples/specimen_websocket_room/Cargo.toml"],
    {
      cwd: new URL("../..", import.meta.url).pathname,
      stdio: ["pipe", "pipe", "inherit"],
    },
  );
  const lines = createInterface({ input: child.stdout });
  for await (const line of lines) {
    const match = /^ROOM_SERVER_ADDR=(.+)$/.exec(line);
    if (match) {
      return { child, addr: match[1] };
    }
  }
  const [code] = await once(child, "exit");
  throw new Error(`room server exited before binding: ${code}`);
}

async function stopServer(child) {
  child.stdin.end();
  await Promise.race([
    once(child, "exit"),
    new Promise((resolve) => setTimeout(resolve, 2000)),
  ]);
  if (!child.killed && child.exitCode === null) {
    child.kill("SIGTERM");
  }
}

test("browser WebSocket page opens, sends, receives, and reports", async ({ page }) => {
  const server = await startServer();
  try {
    await page.goto(`http://${server.addr}/`);
    const log = page.locator("#log");
    await expect(log).toContainText("open:tina.room.v1");
    await expect(log).toContainText(/^join:/m);
    await expect(log).toContainText("binary:3");

    const report = await page.request.get(`http://${server.addr}/room-report`);
    expect(report.ok()).toBeTruthy();
    const body = await report.json();
    expect(body.joined).toBe(1);
    expect(body.live_members).toBe(1);
  } finally {
    await stopServer(server.child);
  }
});
