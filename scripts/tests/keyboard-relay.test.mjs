// Chan's keyboard relay in app.js, executed as shipped. The relay block is
// read out of the asset between its two marker comments and run against an
// isolated frame host, so these are the keydowns a browser would relay.
//
// The keydowns come from published layouts, the same facts Chan's own
// matchers are tested against (Chan's web/packages/web-shared/src/
// keyboardVectors.ts). The advertised chords are a subset of what Chan
// advertises to an extension on a Linux browser.
//
//   node --test scripts/tests/keyboard-relay.test.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const source = readFileSync(
  new URL("../../crates/mobile-chat-extension/assets/app.js", import.meta.url),
  "utf8",
);
const start = source.indexOf("// Chan's keyboard relay.");
const end = source.indexOf("// End of Chan's keyboard relay.");
assert.ok(start >= 0 && end > start, "the relay block is marked in app.js");
const RELAY = source.slice(start, end);

const LINUX = "Mozilla/5.0 (X11; Linux x86_64)";
const MAC = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)";

const chord = (key, mods) => ({
  key,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  shiftKey: false,
  ...mods,
});
const ADVERTISED = [
  chord("K", { ctrlKey: true, altKey: true }), // command launcher
  chord("T", { ctrlKey: true, shiftKey: true }), // new terminal
  chord(".", { ctrlKey: true }), // Hybrid Nav
  chord(",", { ctrlKey: true }), // Settings
  chord("/", { ctrlKey: true, altKey: true, shiftKey: true }), // split down
  chord("1", { ctrlKey: true, altKey: true }), // first tab
];

function load(userAgent = LINUX) {
  const posted = [];
  const listeners = {};
  const parent = { postMessage: (message) => posted.push(message) };
  const frame = {
    parent,
    addEventListener: (type, fn) => (listeners[type] ??= []).push(fn),
  };
  const state = {};
  new Function("window", "navigator", "state", RELAY)(frame, { userAgent }, state);
  return {
    posted,
    state,
    host(data, from = parent) {
      for (const fn of listeners.message ?? []) fn({ source: from, data });
    },
    advertise(keys = ADVERTISED) {
      this.host({ type: "chan:extension-host-keymap:v2", keys });
    },
    press(init) {
      const { altGraph = false, ...fields } = init;
      const event = {
        key: "",
        code: "",
        ctrlKey: false,
        altKey: false,
        metaKey: false,
        shiftKey: false,
        repeat: false,
        isComposing: false,
        defaultPrevented: false,
        ...fields,
        getModifierState: (name) => name === "AltGraph" && altGraph,
        preventDefault() {
          this.defaultPrevented = true;
        },
      };
      for (const fn of listeners.keydown ?? []) fn(event);
      return event;
    },
  };
}

const CTRL_SHIFT = { ctrlKey: true, shiftKey: true };

test("Colemak Ctrl+Shift+T on KeyF is relayed once with its raw fields", () => {
  const relay = load();
  relay.advertise();
  const event = relay.press({ key: "T", code: "KeyF", ...CTRL_SHIFT });
  assert.equal(event.defaultPrevented, true);
  assert.deepEqual(relay.posted, [
    {
      type: "chan:extension-keydown:v2",
      key: "T",
      code: "KeyF",
      ctrlKey: true,
      altKey: false,
      metaKey: false,
      shiftKey: true,
      repeat: false,
      isComposing: false,
      altGraph: false,
    },
  ]);
});

test("the G on KeyT is not relayed and keeps its default", () => {
  const relay = load();
  relay.advertise();
  const event = relay.press({ key: "G", code: "KeyT", ...CTRL_SHIFT });
  assert.equal(event.defaultPrevented, false);
  assert.deepEqual(relay.posted, []);
});

test("Dvorak , on KeyW is relayed; the W on Comma is not", () => {
  const relay = load();
  relay.advertise();
  relay.press({ key: ",", code: "KeyW", ctrlKey: true });
  relay.press({ key: "w", code: "Comma", ctrlKey: true });
  assert.deepEqual(
    relay.posted.map((message) => message.code),
    ["KeyW"],
  );
});

test("AZERTY . typed with Shift reaches the unshifted chord", () => {
  const relay = load();
  relay.advertise();
  relay.press({ key: ".", code: "Comma", ...CTRL_SHIFT });
  assert.equal(relay.posted.length, 1);
});

test("US Ctrl+Shift+. types > and keeps its Shift, so it is not relayed", () => {
  const relay = load();
  relay.advertise();
  relay.press({ key: ">", code: "Period", ...CTRL_SHIFT });
  assert.deepEqual(relay.posted, []);
});

test("an explicitly shifted chord: `?` is Shift plus `/`", () => {
  const relay = load();
  relay.advertise();
  relay.press({ key: "?", code: "Slash", ctrlKey: true, altKey: true, shiftKey: true });
  assert.equal(relay.posted.length, 1);
});

test("AZERTY & on Digit1 keeps the digit position", () => {
  const relay = load();
  relay.advertise();
  relay.press({ key: "&", code: "Digit1", ctrlKey: true, altKey: true });
  assert.equal(relay.posted.length, 1);
});

test("text entry is never relayed", () => {
  const relay = load();
  relay.advertise();
  const launcher = { key: "k", code: "KeyK", ctrlKey: true, altKey: true };
  relay.press({ ...launcher, isComposing: true });
  relay.press({ ...launcher, key: "Process" });
  relay.press({ ...launcher, altGraph: true });
  relay.press({ key: "Dead", code: "BracketLeft", ctrlKey: true });
  assert.deepEqual(relay.posted, []);
});

test("on macOS AltGraph is Option, and an Option glyph falls back to its position", () => {
  const relay = load(MAC);
  relay.advertise([chord("K", { ctrlKey: true, altKey: true })]);
  relay.press({ key: "˚", code: "KeyK", ctrlKey: true, altKey: true, altGraph: true });
  assert.equal(relay.posted.length, 1);
});

test("a keydown the page already handled is not relayed", () => {
  const relay = load();
  relay.advertise();
  relay.press({ key: "T", code: "KeyF", ...CTRL_SHIFT, defaultPrevented: true });
  assert.deepEqual(relay.posted, []);
});

test("nothing is relayed before a keymap, from another window, or on v1", () => {
  const relay = load();
  relay.press({ key: "T", code: "KeyF", ...CTRL_SHIFT });
  relay.host({ type: "chan:extension-host-keymap:v2", keys: ADVERTISED }, {});
  relay.press({ key: "T", code: "KeyF", ...CTRL_SHIFT });
  relay.host({ type: "chan:extension-host-keymap:v1", keys: ADVERTISED });
  relay.press({ key: "T", code: "KeyF", ...CTRL_SHIFT });
  assert.deepEqual(relay.posted, []);
});
