// Run against a disposable Chan server configured with agent-fixture.mjs.
// MOBILE_CHAT_PLAYWRIGHT may name an absolute path to playwright/index.mjs.
import assert from "node:assert/strict";
import { mkdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
const { chromium } = await import(
  process.env.MOBILE_CHAT_PLAYWRIGHT || "playwright"
);
const url = process.env.MOBILE_CHAT_TEST_URL;
const eventFile = process.env.MOBILE_CHAT_TEST_EVENTS;
if (!url || !eventFile)
  throw new Error("Set MOBILE_CHAT_TEST_URL and MOBILE_CHAT_TEST_EVENTS.");
const out = resolve(
  process.env.MOBILE_CHAT_TEST_OUTPUT || "/tmp/mobile-chat-browser",
);
mkdirSync(out, { recursive: true });
const events = () => {
  try {
    return readFileSync(eventFile, "utf8")
      .trim()
      .split("\n")
      .filter(Boolean)
      .map(JSON.parse);
  } catch (error) {
    if (error.code === "ENOENT") return [];
    throw error;
  }
};
const spawnCount = () => events().filter((e) => e.event === "spawn").length;
const browser = await chromium.launch({ headless: true });
const context = await browser.newContext({
  viewport: { width: 390, height: 844 },
});
const page = await context.newPage();
page.setDefaultTimeout(15000);
const errors = [];
page.on("pageerror", (error) => errors.push(error.message));
const frames = () => page.frames().filter((f) => f.parentFrame());
const tabs = () => page.getByRole("tab").filter({ hasText: "Mobile Chat" });
async function open(agent, text) {
  await page.keyboard.press("Control+Alt+k");
  await page
    .getByRole("combobox", { name: "Search", exact: true })
    .fill("mobile chat");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(300);
  const f = frames().at(-1);
  await f.locator("#agent").selectOption(agent);
  await f.locator("#first-message").fill(text);
  await f.locator("#create").click();
  if (agent === "kimi") {
    await f.locator("#connect-agent").waitFor();
    await f.locator("#connect-agent").click();
  }
  return f;
}
async function send(f, body) {
  await f.locator("#text").fill(body);
  await f.locator("#send").click();
}
try {
  await page.goto(url);
  await page.waitForTimeout(1000);
  const initialSpawns = spawnCount();
  const initialTabs = await tabs().count();
  const firstTitle = `survey first-${Date.now()}`;
  let a = await open("claude", firstTitle);
  await a.getByText("Where should we run this?", { exact: true }).waitFor();
  await a.locator("#text").fill("draft survives reload");
  await a.locator(".question-form textarea").fill("partial answer");
  await a.getByText("Saved", { exact: true }).waitFor();
  await page.reload();
  await page.waitForTimeout(800);
  a = frames().at(-1);
  await a.locator(".question-form textarea").waitFor();
  assert.equal(await a.locator("#text").inputValue(), "draft survives reload");
  assert.equal(
    await a.locator(".question-form textarea").inputValue(),
    "partial answer",
  );
  assert.equal(spawnCount(), initialSpawns + 1);
  await a.getByRole("button", { name: "Remote", exact: true }).click();
  await a.getByRole("button", { name: "Send answer", exact: true }).click();
  await a.getByText("Received answer: Remote", { exact: true }).waitFor();
  assert.equal(await a.locator('.entry[data-kind="answer"]').count(), 1);
  await page.screenshot({ path: `${out}/mobile-chat.png` });
  console.log(
    "PASS saved draft, pending question, reply and helper retry deduplication",
  );

  await a.evaluate(() => {
    const send = WebSocket.prototype.send;
    WebSocket.prototype.send = function (data) {
      WebSocket.prototype.send = send;
      this.close();
    };
  });
  await context.setOffline(true);
  await a.locator("#text").fill("offline draft");
  await a
    .getByText("Offline · changes not yet saved", { exact: true })
    .waitFor();
  await context.setOffline(false);
  await a.getByText("Saved", { exact: true }).waitFor();
  assert.equal(await a.locator("#text").inputValue(), "offline draft");
  await a.evaluate(() => {
    const send = WebSocket.prototype.send;
    WebSocket.prototype.send = function (data) {
      send.call(this, data);
      if (JSON.parse(data).op === "send") {
        WebSocket.prototype.send = send;
        this.close();
      }
    };
  });
  const repeated = `delivery-${Date.now()}`;
  await send(a, repeated);
  await a
    .getByText(`Received ${repeated.length} bytes: ${repeated}`, {
      exact: true,
    })
    .waitFor();
  assert.equal(
    events().filter((e) => e.event === "read" && e.body === repeated).length,
    1,
  );
  console.log("PASS offline draft and lost send acknowledgment");

  let b = await open("codex", "many");
  await b
    .getByText("History complete.", { exact: true })
    .waitFor({ timeout: 45000 });
  await page.waitForTimeout(500);
  assert.equal(
    await page
      .getByRole("button", { name: "Flip to side B (Ctrl+`)" })
      .isVisible(),
    true,
  );
  assert.equal(
    await b
      .locator("#messages")
      .getByText(firstTitle, { exact: true })
      .count(),
    0,
  );
  await b.locator("#transcript").evaluate((n) => {
    n.scrollTop = 400;
  });
  await b.getByText("Saved", { exact: true }).waitFor();
  const position = await b.locator("#messages > article").evaluateAll((ns) => {
    const top = document
      .getElementById("transcript")
      .getBoundingClientRect().top;
    const node = ns.find((n) => n.getBoundingClientRect().bottom > top);
    return {
      id: node.dataset.id,
      offset: node.getBoundingClientRect().top - top,
    };
  });
  await page.reload();
  await page.waitForTimeout(800);
  [a, b] = frames().slice(-2);
  await b.locator(`#message-${position.id}`).waitFor();
  const offset = await b
    .locator(`#message-${position.id}`)
    .evaluate(
      (n) =>
        n.getBoundingClientRect().top -
        document.getElementById("transcript").getBoundingClientRect().top,
    );
  assert.ok(Math.abs(offset - position.offset) < 2);
  await send(b, "L".repeat(6000));
  await b
    .locator('.entry[data-role="assistant"]')
    .filter({ hasText: "Received 6000 bytes:" })
    .waitFor();
  console.log(
    "PASS independent chats, paginated reading position, long message, launch focus",
  );

  const c = await open("kimi", "survey manual connection");
  await c.getByText("Where should we run this?", { exact: true }).waitFor();
  await c.locator("#peek").click();
  await page.getByRole("button", { name: "Flip to side A (Ctrl+`)" }).waitFor();
  const focused = await page.getByRole("tab", { selected: true }).innerText();
  const kimiPid = events().findLast((e) => e.event === "spawn").pid;
  assert.match(focused, /@@chat-/);
  await page.getByRole("button", { name: "Flip to side A (Ctrl+`)" }).click();
  // Chan rotates the containing pane for 520 ms; iframe-local actionability
  // cannot observe the parent transform. Allow the host animation to finish.
  await page.waitForTimeout(700);
  await c.locator("#actions summary").click();

  await c.locator("#stop").click();
  await c
    .getByText("Agent stopped · question inactive", { exact: true })
    .waitFor();
  await tabs().nth(initialTabs).click();
  await send(a, "still running");
  await a
    .getByText("Received 13 bytes: still running", { exact: true })
    .waitFor();
  assert.equal(spawnCount(), initialSpawns + 3);
  assert.notEqual(events().findLast((e) => e.event === "read").pid, kimiPid);
  console.log(
    "PASS explicit Kimi connection, Peek and stopping only one agent",
  );

  await page
    .getByRole("button", { name: "close Mobile Chat", exact: true })
    .nth(initialTabs)
    .click();
  await page.keyboard.press("Control+Alt+k");
  await page
    .getByRole("combobox", { name: "Search", exact: true })
    .fill("mobile chat");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(300);
  const reopened = frames().at(-1);
  await reopened
    .locator("#recent button")
    .filter({ hasText: firstTitle })
    .first()
    .click();
  await reopened
    .getByText("Received 13 bytes: still running", { exact: true })
    .waitFor();
  assert.equal(spawnCount(), initialSpawns + 3);
  assert.deepEqual(errors, []);
  console.log("PASS closing and reopening a chat preserves its running agent");
} catch (error) {
  await page.screenshot({ path: `${out}/failure.png` });
  console.log(await page.locator("body").innerText());
  console.log(
    await frames()
      .at(-1)
      .locator("#actions")
      .evaluate((n) => n.outerHTML),
  );
  throw error;
} finally {
  await browser.close();
}
