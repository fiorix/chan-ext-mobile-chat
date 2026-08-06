// Mobile Chat UI.
//
// This runs in an opaque-origin sandbox: no same-origin access to Chan, no
// storage, and every postMessage needs a "*" target origin. The source check on
// every inbound message is what "*" costs us, so it is not optional.

const HOST_READY = "chan:extension-ready:v1";
const HOST_COMMAND = "chan:extension-command:v1";
const HOST_RESULT = "chan:extension-command-result:v1";
const HOST_SESSION = "chan:extension-session-context:v1";
const HOST_VIEW = "chan:extension-view-state:v1";

// `cs terminal write` refuses anything larger, so refuse it here where we can
// say something useful about it.
const MAX_WRITE_BYTES = 4096;

const state = {
  socket: null,
  windowId: null,
  status: null,
  hostReady: false,
};

const el = (id) => document.getElementById(id);
const ui = {
  dot: el("dot"),
  headline: el("headline"),
  detail: el("detail"),
  picker: el("picker"),
  agent: el("agent"),
  start: el("start"),
  peek: el("peek"),
  recovery: el("recovery"),
  composer: el("composer"),
  text: el("text"),
  count: el("count"),
  send: el("send"),
  notice: el("notice"),
};

function controlUrl() {
  const url = new URL("control", window.location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}

function connect() {
  const socket = new WebSocket(controlUrl());
  state.socket = socket;
  socket.addEventListener("open", () => {
    if (state.windowId) send({ op: "hello", window_id: state.windowId });
  });
  socket.addEventListener("message", (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (message.type === "status") {
      state.status = message;
      render();
    } else if (message.type === "notice") {
      showNotice(message);
    }
  });
  socket.addEventListener("close", () => {
    state.socket = null;
    setTimeout(connect, 1500);
  });
}

function send(payload) {
  if (state.socket && state.socket.readyState === WebSocket.OPEN) {
    state.socket.send(JSON.stringify(payload));
    return true;
  }
  showNotice({ ok: false, message: "not connected to the extension" });
  return false;
}

function showNotice({ ok, message }) {
  if (!message) {
    ui.notice.hidden = true;
    return;
  }
  ui.notice.hidden = false;
  ui.notice.textContent = message;
  ui.notice.dataset.ok = String(ok !== false);
}

const HEADLINES = {
  idle: () => "Pick an agent",
  spawning: (s) => `Starting ${s.agent}`,
  booting: (s) => `${s.agent} is booting`,
  live: (s) => `${s.handle} · ${s.agent}`,
  stalled: (s) => `${s.handle} is stuck`,
  dead: (s) => `${s.agent} exited`,
  failed: (s) => `${s.agent || "agent"} failed to start`,
};

function render() {
  const status = state.status;
  if (!status) return;

  const phase = status.phase;
  ui.dot.dataset.phase = phase;
  ui.headline.textContent = (HEADLINES[phase] || (() => phase))(status);

  const bits = [];
  if (status.detail) bits.push(status.detail);
  if (phase === "live" || phase === "stalled") {
    bits.push(`queue ${status.queue_depth}`, `quiet ${status.idle_secs}s`);
  }
  ui.detail.hidden = bits.length === 0;
  ui.detail.textContent = bits.join(" · ");

  const running = phase === "live" || phase === "stalled" || phase === "booting";
  const finished = phase === "idle" || phase === "dead" || phase === "failed";

  ui.picker.hidden = !finished;
  ui.composer.hidden = !running;
  ui.peek.hidden = !running;
  ui.recovery.hidden = !(phase === "stalled" || phase === "dead" || phase === "live");
  ui.send.disabled = phase !== "live" && phase !== "stalled";

  if (ui.agent.options.length !== (status.agents || []).length) {
    ui.agent.replaceChildren(
      ...(status.agents || []).map((name) => {
        const option = document.createElement("option");
        option.value = name;
        option.textContent = name;
        return option;
      }),
    );
  }
  updateCount();
}

function updateCount() {
  const bytes = new TextEncoder().encode(ui.text.value).length;
  const over = bytes > MAX_WRITE_BYTES;
  ui.count.textContent = bytes === 0 ? "" : `${bytes} / ${MAX_WRITE_BYTES} bytes`;
  ui.count.dataset.over = String(over);
  if (over) ui.send.disabled = true;
}

function sendMessage() {
  const text = ui.text.value;
  if (!text.trim()) return;
  if (new TextEncoder().encode(text).length > MAX_WRITE_BYTES) {
    showNotice({
      ok: false,
      message: `Over ${MAX_WRITE_BYTES} bytes. Write it to a file and send the path instead.`,
    });
    return;
  }
  if (send({ op: "send", text })) {
    ui.text.value = "";
    updateCount();
  }
}

ui.start.addEventListener("click", () => {
  if (!state.windowId) {
    showNotice({
      ok: false,
      message: "Chan has not sent this window's session context yet.",
    });
    return;
  }
  send({ op: "start", agent: ui.agent.value, window_id: state.windowId });
});
ui.send.addEventListener("click", sendMessage);
ui.peek.addEventListener("click", () => send({ op: "peek" }));
for (const op of ["nudge", "escape", "restart", "close"]) {
  el(op).addEventListener("click", () => send({ op }));
}
ui.text.addEventListener("input", updateCount);
ui.text.addEventListener("keydown", (event) => {
  // Enter sends; Shift+Enter is a newline. On a phone the on-screen keyboard's
  // return key is the natural send.
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    sendMessage();
  }
});

window.addEventListener("message", (event) => {
  if (event.source !== window.parent) return;
  const message = event.data;
  if (!message || typeof message !== "object") return;

  if (message.type === HOST_SESSION) {
    if (typeof message.self_id === "string" && message.self_id) {
      state.windowId = message.self_id;
      if (state.socket && state.socket.readyState === WebSocket.OPEN) {
        send({ op: "hello", window_id: state.windowId });
      }
    }
  } else if (message.type === HOST_COMMAND && typeof message.id === "string") {
    const ok = message.id === "peek" ? send({ op: "peek" }) : false;
    window.parent.postMessage(
      { type: HOST_RESULT, request_id: message.request_id, ok },
      "*",
    );
  } else if (message.type === HOST_VIEW) {
    // Nothing to pause: the extension does the polling, not this frame.
  }
});

if (!state.hostReady) {
  state.hostReady = true;
  window.parent.postMessage({ type: HOST_READY }, "*");
}

connect();
