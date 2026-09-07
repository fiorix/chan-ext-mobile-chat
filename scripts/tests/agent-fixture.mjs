#!/usr/bin/env node
// Deterministic interactive agent for a disposable Chan workspace.
// Understands Chan's Claude, Codex, and Kimi submit sequences.
import { spawnSync } from "node:child_process";
import { appendFileSync } from "node:fs";

const log = process.argv[2];
const record = (event) => {
  if (log)
    appendFileSync(log, JSON.stringify({ pid: process.pid, ...event }) + "\n");
};
function helper(...args) {
  const result = spawnSync(process.env.MOBILE_CHAT_HELPER, ["agent", ...args], {
    encoding: "utf8",
    timeout: 35000,
  });
  if (result.status !== 0)
    throw new Error(result.stderr || String(result.error));
  return JSON.parse(result.stdout);
}

function prompt(body) {
  const match = /Mobile Chat message ([a-zA-Z0-9_-]+)\./.exec(body);
  if (!match) {
    if (body.includes("You are in Mobile Chat.")) {
      helper("ready");
      record({ event: "ready" });
    }
    return;
  }
  const id = match[1];
  const message = helper("read", id);
  record({
    event: "read",
    id,
    body: message.body,
    question_id: message.question_id,
  });
  if (message.already_read) return;
  if (/survey/i.test(message.body) && !message.question_id) {
    helper(
      "reply",
      "--id",
      `${id}-progress`,
      "--to",
      id,
      "--progress",
      "Checking the options.",
    );
    helper(
      "ask",
      "--id",
      `${id}-question`,
      "--to",
      id,
      "--option",
      "Local",
      "--option",
      "Remote",
      "Where should we run this?",
    );
    helper("ready");
  } else if (message.body === "many") {
    for (let i = 0; i < 75; i++) {
      helper(
        "reply",
        "--id",
        `${id}-${i}`,
        "--to",
        id,
        "--progress",
        `Update ${i}\n\n${"History for scroll restoration. ".repeat(15)}`,
      );
    }
    helper("reply", "--id", `${id}-final`, "--to", id, "History complete.");
  } else {
    const text = message.question_id
      ? `Received answer: ${message.body}`
      : `Received ${Buffer.byteLength(message.body)} bytes: ${message.body}`;
    const args = ["reply", "--id", `${id}-final`, "--to", id, text];
    helper(...args);
    helper(...args);
  }
  process.stdout.write("\r\nReady for the next message.\r\n");
}

record({ event: "spawn", chord: process.env.CHAN_AGENT });
process.stdin.setRawMode(true);
process.stdin.setEncoding("utf8");
process.stdout.write("Mobile Chat test fixture ready.\r\n");
const initial = process.argv.at(-1);
if (initial?.startsWith("You are in Mobile Chat.")) prompt(initial);
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let match;
  while ((match = /\x1b\[27;9;13~|\r/.exec(buffer))) {
    const submitted = buffer
      .slice(0, match.index)
      .replace(/\x1b\[20[01]~/g, "");
    buffer = buffer.slice(match.index + match[0].length);
    try {
      prompt(submitted);
    } catch (error) {
      record({ event: "error", message: error.message });
      process.stderr.write(`${error.stack}\n`);
    }
  }
});
