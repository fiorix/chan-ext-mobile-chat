// Opaque-origin iframe. Durable data belongs to the extension server.
const HOST_READY = "chan:extension-ready:v1";
const HOST_SESSION = "chan:extension-session-context:v1";
const MAX_BODY = 64 * 1024;
const el = (id) => document.getElementById(id);
const ui = Object.fromEntries(
  [
    "title",
    "status",
    "home",
    "peek",
    "actions",
    "stop",
    "notice",
    "welcome",
    "agent",
    "first-message",
    "create",
    "create-form",
    "recent",
    "restore-note",
    "load-errors",
    "whatsapp",
    "whatsapp-body",
    "chat",
    "detail",
    "connect-agent",
    "transcript",
    "messages",
    "older",
    "latest",
    "ended",
    "another",
    "composer",
    "text",
    "send",
    "saved",
  ].map((id) => [id, el(id)]),
);
const state = {
  socket: null,
  connected: false,
  context: null,
  active: null,
  home: true,
  snapshot: null,
  conversation: null,
  entries: new Map(),
  pending: new Map(),
  view: { draft: "", answers: {}, anchor: null, offset: 0, at_bottom: true },
  viewRevision: 0,
  dirty: false,
  saving: false,
  saveTimer: null,
  restoring: false,
  hasMore: false,
  loading: false,
  busy: new Set(),
  retryDelay: 500,
  whatsapp: null,
  waChats: null,
  waAllow: null,
  waNumber: "",
  waTimer: null,
  waChatsLoading: false,
  waAllowLoading: false,
};
const phases = {
  starting: "Starting agent",
  connecting: "Connecting agent",
  ready: "Ready",
  working: "Working",
  waiting: "Waiting for you",
  stopping: "Stopping agent",
  stopped: "Agent stopped",
  failed: "Could not start agent",
};
const ended = (phase) => phase === "stopped" || phase === "failed";
const bytes = (text) => new TextEncoder().encode(text).length;
const id = () =>
  Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) =>
    b.toString(16).padStart(2, "0"),
  ).join("");

function notice(message = "") {
  ui.notice.textContent = message;
  ui.notice.hidden = !message;
}
function request(op, args = {}) {
  if (!state.connected)
    return Promise.reject(
      new Error("Connection lost. Your text is kept here until you reconnect."),
    );
  const payload = { id: id(), op, ...args };
  return new Promise((resolve, reject) => {
    state.pending.set(payload.id, { payload, resolve, reject });
    state.socket.send(JSON.stringify(payload));
  });
}

function controlUrl() {
  const url = new URL("control", window.location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url.toString();
}

function connect() {
  const socket = new WebSocket(controlUrl());
  state.socket = socket;
  socket.addEventListener("open", async () => {
    if (state.socket !== socket) return;
    state.connected = true;
    state.retryDelay = 500;
    renderHeader();
    const retries = [...state.pending.values()];
    try {
      await hello();
      for (const pending of retries) {
        if (state.pending.has(pending.payload.id))
          socket.send(JSON.stringify(pending.payload));
      }
      if (state.dirty && !state.saving) scheduleSave();
    } catch (error) {
      notice(error.message);
    }
  });
  socket.addEventListener("message", ({ data }) => {
    if (state.socket !== socket) return;
    let message;
    try {
      message = JSON.parse(data);
    } catch {
      return;
    }
    if (message.type === "snapshot") applySnapshot(message);
    else if (message.type === "result") {
      const pending = state.pending.get(message.id);
      if (!pending) {
        if (!message.ok) notice(message.error);
        return;
      }
      state.pending.delete(message.id);
      if (message.ok) pending.resolve(message.result);
      else pending.reject(new Error(message.error));
    } else if (typeof message.state === "string") {
      // WhatsApp status pushes carry no "type"; they are the whatsapp_status
      // payload itself.
      applyWhatsappStatus(message);
    }
  });
  socket.addEventListener("close", () => {
    if (state.socket !== socket) return;
    state.connected = false;
    for (const [key, pending] of state.pending) {
      if (["hello", "peek", "history"].includes(pending.payload.op)) {
        state.pending.delete(key);
        pending.reject(
          new Error("Connection lost. Please try again after reconnecting."),
        );
      }
    }
    renderHeader();
    setTimeout(connect, state.retryDelay);
    state.retryDelay = Math.min(state.retryDelay * 2, 8000);
  });
}

async function hello() {
  if (!state.context) return;
  const result = await request("hello", state.context);
  let restore = state.active;
  // window.name survives iframe reload; a restored Chan window needs host identity.
  if (!restore && !result.conversation_id) {
    const match = /^mobile-chat:([a-zA-Z0-9_-]+)$/.exec(window.name);
    if (match) restore = match[1];
  }
  if (restore && restore !== result.conversation_id)
    await request("attach", { conversation_id: restore });
}

function applySnapshot(snapshot) {
  state.snapshot = snapshot;
  const selectedAgent = ui.agent.value;
  if (ui.agent.dataset.roster !== JSON.stringify(snapshot.agents)) {
    ui.agent.replaceChildren(
      ...snapshot.agents.map((agent) => {
        const option = document.createElement("option");
        option.value = agent;
        option.textContent = agent;
        return option;
      }),
    );
    if (snapshot.agents.includes(selectedAgent)) ui.agent.value = selectedAgent;
    ui.agent.dataset.roster = JSON.stringify(snapshot.agents);
  }
  renderRecent(snapshot.conversations);
  ui["restore-note"].hidden =
    snapshot.persistent_host && !!state.context?.tab_id;
  ui["load-errors"].hidden = snapshot.errors.length === 0;
  ui["load-errors"].textContent = snapshot.errors.join("\n");
  const conversation = snapshot.conversation;
  if (conversation) {
    const changed = state.active !== conversation.id;
    if (
      !changed &&
      state.conversation &&
      conversation.revision < state.conversation.revision
    )
      return;
    const oldCount = state.entries.size;
    const position = changed
      ? conversation.view
      : state.home
        ? state.view
        : capturePosition();
    if (changed) {
      clearTimeout(state.saveTimer);
      state.active = conversation.id;
      window.name = `mobile-chat:${conversation.id}`;
      state.home = false;
      state.entries.clear();
      ui.messages.replaceChildren();
      state.dirty = false;
      state.view = structuredClone(conversation.view);
      state.viewRevision = conversation.view_revision;
      state.hasMore = conversation.has_more;
      ui.text.value = state.view.draft;
    } else if (
      !state.dirty &&
      !state.saving &&
      conversation.view_revision >= state.viewRevision
    ) {
      state.view = structuredClone(conversation.view);
      ui.text.value = state.view.draft;
    }
    state.viewRevision = Math.max(
      state.viewRevision,
      conversation.view_revision,
    );
    state.conversation = conversation;
    for (const entry of conversation.entries)
      state.entries.set(entry.id, entry);
    for (const [key, question] of Object.entries(
      conversation.question_states,
    )) {
      if (state.entries.has(key)) state.entries.get(key).question = question;
    }
    renderEntries();
    if (changed) restorePosition(position);
    else applyPosition(position);
    if (!position.at_bottom && state.entries.size > oldCount)
      ui.latest.hidden = false;
  }
  renderHeader();
}

function renderHeader() {
  const conversation = state.conversation;
  const showChat = conversation && !state.home;
  ui.welcome.hidden = !!showChat;
  ui.chat.hidden = !showChat;
  ui.title.textContent = showChat ? conversation.title : "Mobile Chat";
  ui.status.textContent = !state.connected
    ? "Reconnecting..."
    : showChat
      ? `${conversation.agent} · ${phases[conversation.phase] || conversation.phase}`
      : "Your agents, one conversation at a time";
  ui.peek.hidden = !showChat || ended(conversation?.phase);
  ui.peek.disabled = !state.connected;
  ui.actions.hidden = !showChat || ended(conversation?.phase);
  ui.stop.disabled = !state.connected || state.busy.has("stop");
  ui.create.disabled =
    !state.connected || !state.context || state.busy.has("create");
  if (showChat) {
    ui.detail.textContent = conversation.detail;
    ui.detail.hidden = !conversation.detail;
    ui["connect-agent"].hidden = !conversation.can_connect;
    ui["connect-agent"].disabled =
      !state.connected || state.busy.has("connect-agent");
    ui.composer.hidden = ended(conversation.phase);
    ui.ended.hidden = !ended(conversation.phase);
    ui.send.disabled =
      !state.connected ||
      state.busy.has("send") ||
      conversation.phase === "stopping";
    for (const entry of state.entries.values()) {
      if (entry.question)
        updateQuestion(document.getElementById(`message-${entry.id}`), entry);
    }
  }
  renderSaved();
}

function renderSaved() {
  ui.saved.textContent = !state.connected
    ? "Offline · changes not yet saved"
    : state.dirty || state.saving
      ? "Saving..."
      : state.active
        ? "Saved"
        : "";
}

function renderRecent(conversations) {
  const encoded = JSON.stringify(conversations);
  if (ui.recent.dataset.contents === encoded) return;
  ui.recent.dataset.contents = encoded;
  ui.recent.replaceChildren();
  if (!conversations.length) {
    const empty = document.createElement("p");
    empty.className = "muted";
    empty.textContent = "Your conversations will appear here.";
    ui.recent.append(empty);
  }
  for (const conversation of conversations) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "recent-item";
    const title = document.createElement("strong");
    title.textContent = conversation.title;
    const subtitle = document.createElement("small");
    subtitle.textContent = `${conversation.agent} · ${phases[conversation.phase]}${conversation.pending_questions ? ` · ${conversation.pending_questions} waiting` : ""}`;
    button.append(title, subtitle);
    button.addEventListener("click", () => attach(conversation.id));
    ui.recent.append(button);
  }
}

async function attach(conversationId) {
  try {
    await flushView();
    await request("attach", { conversation_id: conversationId });
    state.home = false;
    notice();
    renderHeader();
  } catch (error) {
    notice(error.message);
  }
}

function applyWhatsappStatus(status) {
  state.whatsapp = status;
  if (status.state === "connected") refreshWhatsappData();
  renderWhatsapp();
}

function refreshWhatsappData() {
  refreshWhatsappChats();
  refreshWhatsappAllow();
}

async function refreshWhatsappChats() {
  if (state.waChatsLoading || !state.connected) return;
  state.waChatsLoading = true;
  try {
    const result = await request("whatsapp_chats");
    state.waChats = result.chats;
  } catch (error) {
    notice(error.message);
  } finally {
    state.waChatsLoading = false;
  }
  renderWhatsapp();
}

async function refreshWhatsappAllow() {
  if (state.waAllowLoading || !state.connected) return;
  state.waAllowLoading = true;
  try {
    // No read-only op exists; removing a number that is not listed is a
    // no-op whose reply carries the full allowlist.
    const result = await request("whatsapp_allow", {
      number: "0",
      allow: false,
    });
    state.waAllow = result.allow;
  } catch (error) {
    notice(error.message);
  } finally {
    state.waAllowLoading = false;
  }
  renderWhatsapp();
}

function renderWhatsapp() {
  clearInterval(state.waTimer);
  state.waTimer = null;
  const body = ui["whatsapp-body"];
  body.replaceChildren();
  const status = state.whatsapp;
  if (!status) {
    ui.whatsapp.hidden = true;
    return;
  }
  ui.whatsapp.hidden = false;
  if (status.state === "disabled") {
    const note = document.createElement("p");
    note.className = "muted";
    note.textContent =
      "The WhatsApp bridge is disabled. Set enabled = true under [whatsapp] in mobile-chat.toml and restart Chan.";
    body.append(note);
    return;
  }
  if (status.state === "locked") {
    const note = document.createElement("p");
    note.className = "muted";
    note.textContent =
      "Another Chan home on this machine holds the WhatsApp bridge lock.";
    body.append(note);
    return;
  }
  if (status.state === "connected") renderWhatsappConnected(body, status);
  else renderWhatsappPairing(body, status);
}

function renderWhatsappPairing(body, status) {
  const offline = !state.connected;
  if (status.state === "error" && status.error) {
    const error = document.createElement("p");
    error.className = "wa-error";
    error.textContent = status.error;
    body.append(error);
  }
  const showQr =
    (status.state === "unpaired" || status.state === "pairing") &&
    typeof status.qr_svg === "string" &&
    status.qr_svg.includes("<svg");
  if (showQr) {
    const box = document.createElement("div");
    box.className = "wa-qr";
    // The SVG is generated by the extension server's own QR renderer.
    box.innerHTML = status.qr_svg;
    const countdown = document.createElement("p");
    countdown.className = "muted";
    countdown.id = "wa-countdown";
    const code = document.createElement("textarea");
    code.className = "wa-code";
    code.rows = 2;
    code.readOnly = true;
    code.value = status.qr_raw || "";
    const copy = document.createElement("button");
    copy.type = "button";
    copy.textContent = "Copy code";
    copy.addEventListener("click", () =>
      copyWhatsappCode(status.qr_raw || "", copy),
    );
    const row = document.createElement("div");
    row.className = "wa-row";
    row.append(code, copy);
    body.append(box, countdown, row);
    if (typeof status.qr_expires_at === "number") {
      tickWhatsappCountdown(status.qr_expires_at);
      state.waTimer = setInterval(
        () => tickWhatsappCountdown(status.qr_expires_at),
        1000,
      );
    }
  } else if (status.state !== "error") {
    const pending = document.createElement("p");
    pending.className = "muted";
    pending.textContent = "Generating pairing code...";
    body.append(pending);
  }
  if (typeof status.qr_deep_link === "string" && status.qr_deep_link) {
    const link = document.createElement("a");
    link.href = status.qr_deep_link;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.textContent = "Open in WhatsApp";
    body.append(link);
  }
  const regenerate = document.createElement("button");
  regenerate.type = "button";
  regenerate.textContent = "Regenerate";
  regenerate.disabled = offline || state.busy.has("whatsapp-pair");
  regenerate.addEventListener("click", () =>
    busy("whatsapp-pair", async () => {
      applyWhatsappStatus(await request("whatsapp_pair"));
    }),
  );
  body.append(regenerate);
}

function tickWhatsappCountdown(expiresAt) {
  const node = el("wa-countdown");
  if (!node) return;
  const left = Math.max(0, expiresAt - Math.floor(Date.now() / 1000));
  node.textContent =
    left > 0
      ? `Code refreshes in ${left}s`
      : "Code expired. Regenerate to keep pairing.";
}

function copyWhatsappCode(text, button) {
  const done = () => {
    button.textContent = "Copied";
    setTimeout(() => {
      button.textContent = "Copy code";
    }, 1500);
  };
  if (navigator.clipboard?.writeText)
    navigator.clipboard
      .writeText(text)
      .then(done, () => fallbackCopy(text, done));
  else fallbackCopy(text, done);
}

function fallbackCopy(text, done) {
  const area = document.createElement("textarea");
  area.value = text;
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.append(area);
  area.select();
  try {
    document.execCommand("copy");
    done();
  } catch {}
  area.remove();
}

function renderWhatsappConnected(body, status) {
  const offline = !state.connected;
  const linked = document.createElement("p");
  const who = status.push_name
    ? `${status.push_name} (${status.jid})`
    : status.jid;
  linked.textContent = `Linked: ${who}`;
  const unlink = document.createElement("button");
  unlink.type = "button";
  unlink.className = "danger";
  unlink.textContent = "Unlink";
  unlink.disabled = offline || state.busy.has("whatsapp-unpair");
  unlink.addEventListener("click", () =>
    busy("whatsapp-unpair", async () => {
      state.waChats = null;
      state.waAllow = null;
      applyWhatsappStatus(await request("whatsapp_unpair"));
    }),
  );
  body.append(linked, unlink, whatsappChatsNode(), whatsappAllowNode());
}

function whatsappChatsNode() {
  const section = document.createElement("section");
  const subtitle = document.createElement("h3");
  subtitle.className = "wa-subtitle";
  subtitle.textContent = "Chats";
  section.append(subtitle);
  if (state.waChats === null) {
    const loading = document.createElement("p");
    loading.className = "muted";
    loading.textContent = "Loading chats...";
    section.append(loading);
    return section;
  }
  if (!state.waChats.length) {
    const empty = document.createElement("p");
    empty.className = "muted";
    empty.textContent = "Chats the bridge has seen will appear here.";
    section.append(empty);
    return section;
  }
  for (const chat of state.waChats) section.append(whatsappChatNode(chat));
  return section;
}

function whatsappChatNode(chat) {
  const offline = !state.connected;
  const row = document.createElement("div");
  row.className = "wa-chat";
  const head = document.createElement("div");
  head.className = "wa-chat-head";
  const name = document.createElement("strong");
  name.textContent = chat.name || chat.jid;
  const meta = document.createElement("small");
  const seen = chat.last_seen
    ? new Date(chat.last_seen * 1000).toLocaleDateString()
    : "never seen";
  meta.textContent = `${chat.kind || "chat"} · last seen ${seen}`;
  head.append(name, meta);
  const controls = document.createElement("div");
  controls.className = "wa-chat-controls";
  const record = document.createElement("button");
  record.type = "button";
  record.textContent = "Record";
  record.setAttribute("aria-pressed", String(!!chat.record));
  record.disabled = offline || state.busy.has(`wa-record-${chat.jid}`);
  record.addEventListener("click", () =>
    busy(`wa-record-${chat.jid}`, async () => {
      await request("whatsapp_set_chat", {
        jid: chat.jid,
        record: !chat.record,
      });
      await refreshWhatsappChats();
    }),
  );
  const select = document.createElement("select");
  select.setAttribute("aria-label", "Bound conversation");
  const unbound = document.createElement("option");
  unbound.value = "";
  unbound.textContent = "Not bound";
  select.append(unbound);
  const conversations = state.snapshot?.conversations || [];
  for (const conversation of conversations) {
    const option = document.createElement("option");
    option.value = conversation.id;
    option.textContent = conversation.title;
    select.append(option);
  }
  if (
    chat.conversation &&
    !conversations.some((conversation) => conversation.id === chat.conversation)
  ) {
    const foreign = document.createElement("option");
    foreign.value = chat.conversation;
    foreign.textContent = `Unknown (${chat.conversation})`;
    select.append(foreign);
  }
  select.value = chat.conversation || "";
  select.disabled = offline || state.busy.has(`wa-bind-${chat.jid}`);
  select.addEventListener("change", () =>
    busy(`wa-bind-${chat.jid}`, async () => {
      await request("whatsapp_set_chat", {
        jid: chat.jid,
        conversation_id: select.value,
      });
      await refreshWhatsappChats();
    }),
  );
  controls.append(record, select);
  row.append(head, controls);
  return row;
}

function whatsappAllowNode() {
  const offline = !state.connected;
  const section = document.createElement("section");
  const subtitle = document.createElement("h3");
  subtitle.className = "wa-subtitle";
  subtitle.textContent = "Allowlist";
  const hint = document.createElement("p");
  hint.className = "muted";
  hint.textContent = "Allowed numbers can drive the bound agent with /agent.";
  section.append(subtitle, hint);
  const list = document.createElement("div");
  if (state.waAllow === null) {
    const loading = document.createElement("p");
    loading.className = "muted";
    loading.textContent = "Loading allowlist...";
    list.append(loading);
  } else if (!state.waAllow.length) {
    const empty = document.createElement("p");
    empty.className = "muted";
    empty.textContent = "No numbers allowed.";
    list.append(empty);
  } else {
    for (const number of state.waAllow) {
      const row = document.createElement("div");
      row.className = "wa-allow-row";
      const label = document.createElement("span");
      label.textContent = `+${number}`;
      const remove = document.createElement("button");
      remove.type = "button";
      remove.textContent = "Remove";
      remove.disabled = offline || state.busy.has(`wa-allow-${number}`);
      remove.addEventListener("click", () =>
        busy(`wa-allow-${number}`, async () => {
          const result = await request("whatsapp_allow", {
            number,
            allow: false,
          });
          state.waAllow = result.allow;
          renderWhatsapp();
        }),
      );
      row.append(label, remove);
      list.append(row);
    }
  }
  section.append(list);
  const form = document.createElement("form");
  form.className = "wa-allow-form";
  const input = document.createElement("input");
  input.id = "wa-number";
  input.type = "tel";
  input.inputMode = "tel";
  input.autocomplete = "off";
  input.placeholder = "Phone number, e.g. +44 7700 900123";
  input.value = state.waNumber;
  input.addEventListener("input", () => {
    state.waNumber = input.value;
  });
  const allow = document.createElement("button");
  allow.type = "submit";
  allow.className = "primary";
  allow.textContent = "Allow";
  allow.disabled = offline || state.busy.has("wa-allow-add");
  form.append(input, allow);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!state.waNumber.replace(/\D/g, "")) return;
    busy("wa-allow-add", async () => {
      const result = await request("whatsapp_allow", {
        number: state.waNumber,
        allow: true,
      });
      state.waAllow = result.allow;
      state.waNumber = "";
      renderWhatsapp();
    });
  });
  section.append(form);
  return section;
}

function renderEntries() {
  let cursor = ui.messages.firstElementChild;
  for (const entry of state.entries.values()) {
    let node = document.getElementById(`message-${entry.id}`);
    if (!node) {
      node = document.createElement("article");
      node.id = `message-${entry.id}`;
      node.className = "entry";
      node.dataset.id = entry.id;
      node.dataset.role = entry.role;
      node.dataset.kind = entry.kind;
      const header = document.createElement("div");
      header.className = "entry-header";
      header.textContent =
        entry.role === "user" ? "You" : state.conversation.agent;
      const body = document.createElement("div");
      body.className = "message-body";
      // HTML is generated by the server's restricted Markdown renderer.
      body.innerHTML = entry.html;
      for (const link of body.querySelectorAll("a")) {
        link.target = "_blank";
        link.rel = "noopener noreferrer";
      }
      const delivery = document.createElement("p");
      delivery.className = "delivery";
      node.append(header, body, delivery);
      if (entry.question) addQuestion(node, entry);
    }
    const delivery = node.querySelector(".delivery");
    const labels = {
      saved: "Waiting to send",
      sending: "Sending",
      queued: "Queued",
      read: "Read by agent",
      uncertain: "Delivery uncertain",
      failed: "Not delivered",
    };
    delivery.textContent = [labels[entry.delivery], entry.detail]
      .filter(Boolean)
      .join(" · ");
    delivery.dataset.state = entry.delivery || "";
    delivery.hidden = !delivery.textContent;
    if (entry.question) updateQuestion(node, entry);
    if (node !== cursor) ui.messages.insertBefore(node, cursor);
    cursor = node.nextElementSibling;
  }
  ui.older.hidden = !state.hasMore;
  ui.older.disabled = state.loading;
}

function addQuestion(node, entry) {
  const form = document.createElement("form");
  form.className = "question-form";
  const options = document.createElement("div");
  options.className = "question-options";
  const label = document.createElement("label");
  label.htmlFor = `answer-${entry.id}`;
  label.textContent = "Your answer";
  const input = document.createElement("textarea");
  input.id = label.htmlFor;
  input.rows = 2;
  input.placeholder = "Choose above or write your answer";
  input.value = state.view.answers[entry.id] || "";
  for (const option of entry.question.options) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = option;
    button.addEventListener("click", () => {
      input.value = option;
      editAnswer(entry.id, input.value);
      updateQuestion(node, state.entries.get(entry.id));
    });
    options.append(button);
  }
  input.addEventListener("input", () => editAnswer(entry.id, input.value));
  const footer = document.createElement("div");
  footer.className = "question-footer";
  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.textContent = "Cancel question";
  cancel.addEventListener("click", () => answer(entry.id, true));
  const submit = document.createElement("button");
  submit.type = "submit";
  submit.className = "primary";
  submit.textContent = "Send answer";
  footer.append(cancel, submit);
  form.append(options, label, input, footer);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    answer(entry.id, false);
  });
  const status = document.createElement("p");
  status.className = "question-state";
  node.append(form, status);
}

function updateQuestion(node, entry) {
  if (!node) return;
  const pending = entry.question.status === "pending";
  node.querySelector(".question-form").hidden = !pending;
  node.querySelector(".question-state").textContent = {
    pending: "Waiting for your answer",
    answered: "Answered",
    cancelled: "Cancelled",
    inactive: "Agent stopped · question inactive",
  }[entry.question.status];
  const input = node.querySelector("textarea");
  if (document.activeElement !== input)
    input.value = state.view.answers[entry.id] || "";
  for (const button of node.querySelectorAll(".question-options button"))
    button.setAttribute(
      "aria-pressed",
      String(button.textContent === input.value),
    );
  for (const control of node.querySelectorAll("button, textarea"))
    control.disabled = !state.connected || state.busy.has(entry.id);
}

function editAnswer(questionId, text) {
  state.view.answers[questionId] = text;
  state.dirty = true;
  scheduleSave();
}
async function answer(questionId, cancel) {
  const text = cancel
    ? "Question cancelled. Do not proceed with work that requires this answer."
    : state.view.answers[questionId] || "";
  if (!text.trim()) return;
  await busy(questionId, async () => {
    await request("answer", {
      conversation_id: state.active,
      question_id: questionId,
      text,
      cancel,
    });
    delete state.view.answers[questionId];
    state.dirty = true;
    scheduleSave();
  });
}

function capturePosition() {
  const scroller = ui.transcript;
  const atBottom =
    scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 40;
  const top = scroller.getBoundingClientRect().top;
  const anchor = [...ui.messages.children].find(
    (node) => node.getBoundingClientRect().bottom > top,
  );
  return {
    at_bottom: atBottom,
    anchor: anchor?.dataset.id || null,
    offset: anchor ? anchor.getBoundingClientRect().top - top : 0,
  };
}
function applyPosition(position) {
  if (position.at_bottom) {
    ui.transcript.scrollTop = ui.transcript.scrollHeight;
    ui.latest.hidden = true;
  } else if (position.anchor) {
    const node = document.getElementById(`message-${position.anchor}`);
    if (node)
      ui.transcript.scrollTop +=
        node.getBoundingClientRect().top -
        ui.transcript.getBoundingClientRect().top -
        position.offset;
  }
}
async function restorePosition(position) {
  state.restoring = true;
  try {
    while (
      !position.at_bottom &&
      position.anchor &&
      !state.entries.has(position.anchor) &&
      state.hasMore &&
      !state.loading
    )
      await loadEarlier();
    requestAnimationFrame(() => {
      applyPosition(position);
      state.restoring = false;
    });
  } catch (error) {
    state.restoring = false;
    notice(error.message);
  }
}
async function loadEarlier() {
  if (state.loading || !state.hasMore || !state.entries.size) return;
  state.loading = true;
  try {
    const position = capturePosition();
    const before = state.entries.keys().next().value;
    const page = await request("history", {
      conversation_id: state.active,
      before,
    });
    state.entries = new Map([
      ...page.entries.map((entry) => [entry.id, entry]),
      ...state.entries,
    ]);
    state.hasMore = page.has_more;
    renderEntries();
    applyPosition(position);
  } finally {
    state.loading = false;
    ui.older.disabled = false;
  }
}

function scheduleSave() {
  renderSaved();
  clearTimeout(state.saveTimer);
  state.saveTimer = setTimeout(() => {
    saveView().catch((error) => notice(error.message));
  }, 300);
}
async function saveView() {
  if (!state.dirty || state.saving || !state.active || !state.connected) return;
  state.saving = true;
  const view = structuredClone(state.view);
  const encoded = JSON.stringify(view);
  const conversationId = state.active;
  try {
    const result = await request("save_view", {
      conversation_id: conversationId,
      revision: state.viewRevision,
      view,
    });
    if (state.active === conversationId) {
      state.viewRevision = Math.max(state.viewRevision, result.view_revision);
      if (JSON.stringify(state.view) === encoded) state.dirty = false;
    }
  } finally {
    state.saving = false;
    renderSaved();
  }
  if (state.dirty) scheduleSave();
}
async function busy(key, action) {
  if (state.busy.has(key)) return;
  state.busy.add(key);
  renderHeader();
  renderWhatsapp();
  if (state.conversation) renderEntries();
  try {
    notice();
    await action();
  } catch (error) {
    notice(error.message);
  } finally {
    state.busy.delete(key);
    renderHeader();
    renderWhatsapp();
    if (state.conversation) renderEntries();
  }
}

async function flushView() {
  if (!state.dirty && !state.saving) return;
  await saveView();
  if (state.dirty || state.saving)
    throw new Error(
      "Wait for your draft to save before opening another conversation.",
    );
}

ui["create-form"].addEventListener("submit", (event) => {
  event.preventDefault();
  const text = ui["first-message"].value;
  if (!text.trim()) return;
  if (bytes(text) > MAX_BODY) {
    notice("Message is too long (maximum 64 KiB).");
    return;
  }
  busy("create", async () => {
    await flushView();
    const result = await request("create", { agent: ui.agent.value, text });
    state.home = false;
    window.name = `mobile-chat:${result.conversation_id}`;
    if (ui["first-message"].value === text) ui["first-message"].value = "";
  });
});
ui.composer.addEventListener("submit", (event) => {
  event.preventDefault();
  const text = ui.text.value;
  if (!text.trim()) return;
  if (bytes(text) > MAX_BODY) {
    notice("Message is too long (maximum 64 KiB).");
    return;
  }
  busy("send", async () => {
    await request("send", { conversation_id: state.active, text });
    if (ui.text.value === text) {
      ui.text.value = "";
      state.view.draft = "";
      state.dirty = true;
      scheduleSave();
    }
  });
});
ui.text.addEventListener("input", () => {
  state.view.draft = ui.text.value;
  state.dirty = true;
  scheduleSave();
});
ui.text.addEventListener("keydown", (event) => {
  // Return composes on a phone. Desktop Ctrl/Cmd+Enter sends.
  if (
    event.key === "Enter" &&
    (event.ctrlKey || event.metaKey) &&
    !event.isComposing
  ) {
    event.preventDefault();
    ui.composer.requestSubmit();
  }
});
ui.home.addEventListener("click", () => {
  state.home = !state.home;
  renderHeader();
});
ui.another.addEventListener("click", () => {
  state.home = true;
  renderHeader();
  ui["first-message"].focus();
});
ui.peek.addEventListener("click", () =>
  busy("peek", async () => {
    const target = await request("peek", { conversation_id: state.active });
    window.parent.postMessage(
      { type: "chan:extension-focus-terminal:v1", ...target },
      "*",
    );
  }),
);
ui.stop.addEventListener("click", () =>
  busy("stop", async () => {
    ui.actions.open = false;
    await request("stop", { conversation_id: state.active });
  }),
);
ui["connect-agent"].addEventListener("click", () =>
  busy("connect-agent", () =>
    request("connect_agent", { conversation_id: state.active }),
  ),
);
ui.older.addEventListener("click", () =>
  loadEarlier().catch((error) => notice(error.message)),
);
ui.latest.addEventListener("click", () => {
  applyPosition({ at_bottom: true });
});
ui.transcript.addEventListener(
  "scroll",
  () => {
    if (state.restoring || !state.active || state.home) return;
    const position = capturePosition();
    if (position.at_bottom) ui.latest.hidden = true;
    Object.assign(state.view, position);
    state.dirty = true;
    scheduleSave();
  },
  { passive: true },
);
window.addEventListener("pagehide", () => {
  saveView().catch(() => {});
});
document.addEventListener("visibilitychange", () => {
  if (document.hidden) saveView().catch(() => {});
});
window.addEventListener("message", (event) => {
  if (event.source !== window.parent) return;
  const message = event.data;
  if (!message || typeof message !== "object") return;
  if (message.type === HOST_SESSION && typeof message.self_id === "string") {
    state.context = {
      window_id: message.self_id,
      tab_id: message.tab_id || null,
      pane_id: message.pane_id || null,
    };
    renderHeader();
    if (state.connected) hello().catch((error) => notice(error.message));
  } else if (
    message.type === "chan:extension-host-keymap:v1" &&
    Array.isArray(message.keys)
  )
    state.hostKeys = message.keys;
});
window.addEventListener("keydown", (event) => {
  if (
    event.defaultPrevented ||
    !state.hostKeys?.some((key) =>
      ["code", "ctrlKey", "altKey", "metaKey", "shiftKey"].every(
        (field) => key[field] === event[field],
      ),
    )
  )
    return;
  event.preventDefault();
  window.parent.postMessage(
    {
      type: "chan:extension-keydown:v1",
      code: event.code,
      key: event.key,
      ctrlKey: event.ctrlKey,
      altKey: event.altKey,
      metaKey: event.metaKey,
      shiftKey: event.shiftKey,
      repeat: event.repeat,
    },
    "*",
  );
});
window.parent.postMessage({ type: HOST_READY }, "*");
connect();
