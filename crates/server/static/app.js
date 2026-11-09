const app = document.querySelector("#app");
const toast = document.querySelector("#toast");
const remoteAudio = document.querySelector("#remote-audio");

const state = {
  token: sessionStorage.getItem("self-dis-token"),
  me: null,
  channels: [],
  online: [],
  voice: [],
  messages: [],
  seen: new Set(),
  textId: null,
  socket: null,
  pc: null,
  localStream: null,
  voiceChannelId: null,
  voiceLabel: "",
  muted: false,
  answerWait: null,
  remoteIce: [],
  iceOpen: false,
  iceQueue: [],
  voiceEpoch: 0,
};

boot();

async function boot() {
  if (!state.token) {
    renderLogin();
    return;
  }
  const me = await api("/api/me");
  if (!me.ok) {
    sessionStorage.removeItem("self-dis-token");
    state.token = null;
    renderLogin();
    return;
  }
  state.me = await me.json();
  renderShell();
  connect();
}

function renderLogin(message = "") {
  app.innerHTML = "";
  const wrap = el("section", "login");
  const form = el("form");
  form.innerHTML = `
    <h1 class="wordmark">self-dis</h1>
    <p class="lede">Закрытая комната. Логин выдаёт администратор.</p>
    <label>Имя <input name="username" autocomplete="username" required></label>
    <label>Пароль <input name="password" type="password" autocomplete="current-password" required></label>
    <button class="primary" type="submit">Войти</button>
    <p class="login-error"></p>
  `;
  form.querySelector(".login-error").textContent = message;
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const data = new FormData(form);
    const response = await api("/api/login", {
      method: "POST",
      body: JSON.stringify({
        username: data.get("username"),
        password: data.get("password"),
      }),
    });
    if (!response.ok) {
      const body = await response.json().catch(() => null);
      form.querySelector(".login-error").textContent =
        body?.error?.message || "не удалось войти";
      return;
    }
    const session = await response.json();
    state.token = session.token;
    state.me = session.user;
    sessionStorage.setItem("self-dis-token", session.token);
    renderShell();
    connect();
  });
  wrap.append(form);
  app.append(wrap);
}

function renderShell() {
  app.innerHTML = "";
  const shell = el("div", "shell");
  shell.innerHTML = `
    <aside>
      <div>
        <h1 class="wordmark">self-dis</h1>
        <div class="who"></div>
      </div>
      <div>
        <p class="section-label">Кто здесь</p>
        <div class="online"></div>
      </div>
      <div>
        <p class="section-label">Каналы</p>
        <div class="channel-list"></div>
      </div>
      <form class="new-room">
        <input name="room" maxlength="64" placeholder="Новая голосовая">
        <button type="submit">Создать</button>
      </form>
      <div class="voice-bar"></div>
      <button class="logout" type="button">Выйти</button>
    </aside>
    <main>
      <header class="chat-head"><h2></h2></header>
      <div class="messages"></div>
      <form class="composer">
        <textarea name="body" maxlength="4000" placeholder="Сообщение"></textarea>
        <button type="submit">Отправить</button>
      </form>
    </main>
  `;
  shell.querySelector(".logout").addEventListener("click", logout);
  shell.querySelector(".new-room").addEventListener("submit", (event) => {
    event.preventDefault();
    const input = event.target.room;
    const name = input.value.trim();
    if (!name) return;
    send({ type: "create_voice_channel", name });
    input.value = "";
  });
  shell.querySelector(".composer").addEventListener("submit", (event) => {
    event.preventDefault();
    submitMessage(event.target.body);
  });
  const area = shell.querySelector("textarea");
  area.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submitMessage(area);
    }
  });
  app.append(shell);
  paint();
}

function submitMessage(area) {
  const body = area.value.trim();
  if (!body || state.textId == null) return;
  send({ type: "send_message", channel_id: state.textId, body });
  area.value = "";
}

function connect() {
  const protocol = location.protocol === "https:" ? "wss" : "ws";
  const socket = new WebSocket(`${protocol}://${location.host}/api/ws`);
  state.socket = socket;
  socket.addEventListener("open", () => {
    socket.send(JSON.stringify({ type: "auth", token: state.token }));
  });
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    handleServer(message);
  });
  socket.addEventListener("close", () => {
    if (state.socket !== socket || !state.token) return;
    hangup(false);
    showToast("связь прервалась, подключаюсь снова");
    setTimeout(connect, 1000);
  });
}

function handleServer(message) {
  switch (message.type) {
    case "ready":
      if (message.protocol !== 1) {
        showToast("сервер говорит на другой версии протокола");
        return;
      }
      state.me = message.user;
      state.channels = message.channels;
      state.online = message.online;
      state.voice = message.voice;
      if (!state.channels.some((channel) => channel.id === state.textId && channel.kind === "text")) {
        state.textId = state.channels.find((channel) => channel.kind === "text")?.id ?? null;
      }
      paint();
      if (state.textId != null) loadHistory(state.textId);
      break;
    case "error":
      if (state.answerWait) {
        state.answerWait.reject(new Error(message.message));
        state.answerWait = null;
      } else if (state.voiceChannelId != null && !state.pc) {
        hangup(false);
      }
      showToast(message.message);
      break;
    case "message_created":
      if (message.message.channel_id !== state.textId) return;
      appendMessage(message.message);
      break;
    case "channel_created":
    case "channel_updated":
      upsertChannel(message.channel);
      paint();
      break;
    case "channel_deleted":
      state.channels = state.channels.filter((channel) => channel.id !== message.channel_id);
      if (state.voiceChannelId === message.channel_id) hangup(false);
      paint();
      break;
    case "user_online":
      if (!state.online.some((user) => user.id === message.user.id)) {
        state.online.push(message.user);
        paintOnline();
      }
      break;
    case "user_offline":
      state.online = state.online.filter((user) => user.id !== message.user_id);
      paintOnline();
      break;
    case "voice_state":
      applyVoice(message);
      break;
    case "rtc_answer":
      if (state.answerWait) {
        state.answerWait.resolve(message.sdp);
        state.answerWait = null;
      }
      break;
    case "rtc_offer":
      acceptServerOffer(message.sdp);
      break;
    case "rtc_ice":
      addRemoteIce(message);
      break;
    default:
      break;
  }
}

async function loadHistory(channelId) {
  const response = await api(`/api/channels/${channelId}/messages?limit=50`);
  if (!response.ok || channelId !== state.textId) return;
  const messages = await response.json();
  state.messages = [];
  state.seen.clear();
  for (const message of messages) appendMessage(message, false);
  const box = document.querySelector(".messages");
  if (box) box.scrollTop = box.scrollHeight;
}

function appendMessage(message, scroll = true) {
  if (state.seen.has(message.id)) return;
  state.seen.add(message.id);
  state.messages.push(message);
  const box = document.querySelector(".messages");
  if (!box) return;
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 48;
  box.append(renderMessage(message));
  if (scroll && atBottom) box.scrollTop = box.scrollHeight;
}

function renderMessage(message) {
  const row = el("article", "message");
  const who = el("strong", null, message.username);
  const time = document.createElement("time");
  time.dateTime = message.created_at;
  time.textContent = formatTime(message.created_at);
  const meta = el("div");
  meta.append(who, " ", time);
  row.append(meta, el("p", null, message.body));
  return row;
}

function paint() {
  const head = document.querySelector(".chat-head h2");
  const channel = state.channels.find((item) => item.id === state.textId);
  if (head) head.textContent = channel ? channel.name : "Чат";
  document.querySelector(".who").textContent = state.me
    ? `${state.me.username} · ${state.me.role === "admin" ? "админ" : "участник"}`
    : "";
  document.querySelector(".new-room").hidden = state.me?.role !== "admin";
  paintOnline();
  paintChannels();
  paintMessages();
  paintVoice();
}

function paintOnline() {
  const box = document.querySelector(".online");
  if (!box) return;
  box.replaceChildren(
    ...[...state.online]
      .sort((a, b) => a.username.localeCompare(b.username, "ru"))
      .map((user) => el("div", "person", user.username)),
  );
}

function paintChannels() {
  const box = document.querySelector(".channel-list");
  if (!box) return;
  box.replaceChildren();
  for (const channel of state.channels) {
    const row = el("div", "channel");
    if (channel.kind === "text" && channel.id === state.textId) row.classList.add("active");
    if (channel.kind === "voice" && channel.id === state.voiceChannelId) row.classList.add("active");
    const button = el("button", "channel-name", channel.name);
    button.type = "button";
    button.addEventListener("click", () => {
      if (channel.kind === "text") {
        state.textId = channel.id;
        state.messages = [];
        state.seen.clear();
        paint();
        loadHistory(channel.id);
      } else {
        joinVoice(channel.id);
      }
    });
    row.append(button);
    if (state.me?.role === "admin" && channel.kind === "voice") {
      const rename = el("button", "icon-button", "имя");
      rename.type = "button";
      rename.addEventListener("click", () => {
        const name = prompt("Новое название", channel.name);
        if (name && name.trim()) {
          send({ type: "rename_channel", channel_id: channel.id, name: name.trim() });
        }
      });
      const remove = el("button", "icon-button", "убрать");
      remove.type = "button";
      remove.addEventListener("click", () => {
        if (confirm(`Убрать комнату «${channel.name}»?`)) {
          send({ type: "delete_channel", channel_id: channel.id });
        }
      });
      row.append(rename, remove);
    }
    box.append(row);
    if (channel.kind === "voice") {
      const people = el("div", "people");
      for (const person of participants(channel.id)) {
        const line = el(
          "div",
          "person",
          person.muted ? `${person.username} · микрофон выключен` : person.username,
        );
        people.append(line);
      }
      box.append(people);
    }
  }
}

function paintMessages() {
  const box = document.querySelector(".messages");
  if (!box) return;
  box.replaceChildren(...state.messages.map(renderMessage));
  box.scrollTop = box.scrollHeight;
}

function paintVoice() {
  const bar = document.querySelector(".voice-bar");
  if (!bar) return;
  bar.replaceChildren();
  if (state.voiceChannelId == null) {
    bar.append(el("span", null, "Голос не подключён"));
    return;
  }
  const channel = state.channels.find((item) => item.id === state.voiceChannelId);
  bar.append(el("strong", null, channel?.name || "Голос"));
  bar.append(el("span", null, state.voiceLabel));
  const actions = el("div", "voice-actions");
  const mute = el("button", null, state.muted ? "Включить микрофон" : "Выключить микрофон");
  mute.type = "button";
  mute.addEventListener("click", toggleMute);
  const leave = el("button", null, "Покинуть комнату");
  leave.type = "button";
  leave.addEventListener("click", () => hangup(true));
  actions.append(mute, leave);
  bar.append(actions);
}

function participants(channelId) {
  return state.voice.find((room) => room.channel_id === channelId)?.participants ?? [];
}

function applyVoice(message) {
  state.voice = state.voice.filter((room) => room.channel_id !== message.channel_id);
  if (message.participants.length) state.voice.push(message);
  const me = message.participants.find((person) => person.user_id === state.me.id);
  if (state.voiceChannelId === message.channel_id && !me) hangup(false);
  paintChannels();
  paintVoice();
}

function upsertChannel(channel) {
  const index = state.channels.findIndex((item) => item.id === channel.id);
  if (index === -1) state.channels.push(channel);
  else state.channels[index] = channel;
  state.channels.sort((a, b) => a.position - b.position || a.id - b.id);
}

async function joinVoice(channelId) {
  if (state.voiceChannelId === channelId) return;
  hangup(true);
  const epoch = state.voiceEpoch;
  send({ type: "join_voice", channel_id: channelId });
  state.voiceChannelId = channelId;
  state.voiceLabel = "запрашиваю микрофон";
  paintVoice();
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true },
      video: false,
    });
  } catch (error) {
    if (epoch !== state.voiceEpoch) return;
    hangup(true);
    showToast("нет доступа к микрофону");
    return;
  }
  if (epoch !== state.voiceEpoch) {
    for (const track of stream.getTracks()) track.stop();
    return;
  }
  const pc = new RTCPeerConnection({
    iceServers:
      location.hostname === "localhost" || location.hostname === "127.0.0.1"
        ? []
        : [{ urls: "stun:stun.l.google.com:19302" }],
  });
  state.pc = pc;
  state.localStream = stream;
  state.remoteIce = [];
  state.iceOpen = false;
  state.iceQueue = [];
  pc.addTrack(stream.getAudioTracks()[0], stream);
  pc.ontrack = (event) => {
    const audio = document.createElement("audio");
    audio.autoplay = true;
    audio.srcObject = event.streams[0] ?? new MediaStream([event.track]);
    remoteAudio.append(audio);
    audio.play().catch(() => {});
  };
  pc.onicecandidate = (event) => {
    const payload = {
      type: "rtc_ice",
      candidate: event.candidate ? event.candidate.candidate : null,
      sdp_mid: event.candidate?.sdpMid ?? null,
      sdp_mline_index: event.candidate?.sdpMLineIndex ?? null,
    };
    if (state.iceOpen) send(payload);
    else state.iceQueue.push(payload);
  };
  pc.oniceconnectionstatechange = () => {
    state.voiceLabel = iceLabel(pc.iceConnectionState);
    paintVoice();
  };
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  const answer = waitForAnswer();
  send({ type: "rtc_offer", sdp: offer.sdp });
  state.iceOpen = true;
  for (const payload of state.iceQueue) send(payload);
  state.iceQueue = [];
  try {
    const sdp = await answer;
    if (state.pc !== pc) return;
    await pc.setRemoteDescription({ type: "answer", sdp });
    for (const candidate of state.remoteIce) await pc.addIceCandidate(candidate);
    state.remoteIce = [];
    state.voiceLabel = iceLabel(pc.iceConnectionState);
    paintVoice();
  } catch (error) {
    if (state.pc === pc) hangup(true);
    showToast(error.message || "не удалось подключить голос");
  }
}

function waitForAnswer() {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      if (state.answerWait) {
        state.answerWait = null;
        reject(new Error("сервер не ответил на голос"));
      }
    }, 8000);
    state.answerWait = {
      resolve: (sdp) => {
        clearTimeout(timer);
        resolve(sdp);
      },
      reject: (error) => {
        clearTimeout(timer);
        reject(error);
      },
    };
  });
}

async function acceptServerOffer(sdp) {
  const pc = state.pc;
  if (!pc) return;
  try {
    await pc.setRemoteDescription({ type: "offer", sdp });
    const answer = await pc.createAnswer();
    await pc.setLocalDescription(answer);
    send({ type: "rtc_answer", sdp: answer.sdp });
  } catch (error) {
    showToast("не удалось обновить голосовое соединение");
  }
}

function addRemoteIce(message) {
  if (!message.candidate) return;
  const candidate = {
    candidate: message.candidate,
    sdpMid: message.sdp_mid,
    sdpMLineIndex: message.sdp_mline_index,
  };
  const pc = state.pc;
  if (!pc?.remoteDescription) {
    state.remoteIce.push(candidate);
    return;
  }
  pc.addIceCandidate(candidate).catch(() => {});
}

function toggleMute() {
  state.muted = !state.muted;
  for (const track of state.localStream?.getAudioTracks() ?? []) {
    track.enabled = !state.muted;
  }
  send({ type: "set_mute", muted: state.muted });
  paintVoice();
}

function hangup(tellServer) {
  state.voiceEpoch += 1;
  if (tellServer && state.voiceChannelId != null) send({ type: "leave_voice" });
  state.voiceChannelId = null;
  state.voiceLabel = "";
  state.muted = false;
  state.answerWait = null;
  state.pc?.close();
  state.pc = null;
  for (const track of state.localStream?.getTracks() ?? []) track.stop();
  state.localStream = null;
  remoteAudio.replaceChildren();
  paintVoice();
  paintChannels();
}

function iceLabel(value) {
  if (value === "connected" || value === "completed") return "в эфире";
  if (value === "checking" || value === "new") return "соединяюсь";
  if (value === "disconnected" || value === "failed") return "нет связи";
  return value || "";
}

async function logout() {
  hangup(true);
  await api("/api/logout", { method: "POST" });
  sessionStorage.removeItem("self-dis-token");
  state.token = null;
  state.socket?.close();
  state.socket = null;
  renderLogin();
}

function send(message) {
  if (state.socket?.readyState === WebSocket.OPEN) {
    state.socket.send(JSON.stringify(message));
  }
}

async function api(path, options = {}) {
  const headers = new Headers(options.headers);
  if (options.body) headers.set("content-type", "application/json");
  if (state.token) headers.set("authorization", `Bearer ${state.token}`);
  return fetch(path, { ...options, headers });
}

function showToast(message) {
  toast.hidden = false;
  toast.textContent = message;
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => {
    toast.hidden = true;
  }, 3200);
}

function formatTime(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleTimeString("ru", { hour: "2-digit", minute: "2-digit" });
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}
