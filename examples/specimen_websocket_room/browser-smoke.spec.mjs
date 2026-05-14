import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createInterface } from "node:readline";

async function startServer(tls = false) {
  const child = spawn(
    "cargo",
    ["run", "--quiet", "--manifest-path", "examples/specimen_websocket_room/Cargo.toml"],
    {
      cwd: new URL("../..", import.meta.url).pathname,
      env: { ...process.env, ...(tls ? { ROOM_SERVER_TLS: "1" } : {}) },
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

async function proveBrowserRoom(page, server, scheme) {
  await page.goto(`${scheme}://${server.addr}/`);
  const log = page.locator("#log");
  await expect(log).toContainText("open:tina.room.v1");
  await expect(log).toContainText(/^join:/m);
  await expect(log).toContainText("binary:3");
  await expect(log).toContainText(/^close:/m);

  const response = await page.evaluate(async () => {
    const report = await fetch("/room-report");
    return { ok: report.ok, body: await report.json() };
  });
  expect(response.ok).toBeTruthy();
  const body = response.body;
  expect(body.joined).toBe(1);
  expect(body.live_members).toBe(0);
  expect(body.selected_subprotocol_seen).toBe(true);
  expect(body.peer_close_seen).toBe(true);
}

test("browser ws page opens, sends, receives, and reports", async ({ page }) => {
  const server = await startServer(false);
  try {
    await proveBrowserRoom(page, server, "http");
  } finally {
    await stopServer(server.child);
  }
});

test("browser wss page opens, sends, receives, and reports", async ({ browser }) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  const page = await context.newPage();
  const server = await startServer(true);
  try {
    await proveBrowserRoom(page, server, "https");
  } finally {
    await stopServer(server.child);
    await context.close();
  }
});
