const $ = (id) => document.getElementById(id);
const canvas = $("video");
const context = canvas.getContext("2d");
const mac = navigator.platform.startsWith("Mac");
const appWindow = window.__TAURI__.window.getCurrentWindow();

let phones = [];
let selected = null;
// a new object per stream, so an old stream ending can't touch a newer one of the same phone
let showing = null;
let failed = null;
let decoder = null;
let description = "";
let frameCount = 0;
let fpsSince = performance.now();

async function refreshPhones() {
  const found = await window.bridge.phones();
  phones = Array.isArray(found) ? found : [];
  if (!Array.isArray(found)) empty("No iPhone found", found.error, true);
  if (!phones.some((p) => p.udid === failed)) failed = null;
  if (!phones.some((p) => p.udid === selected)) selected = phones[0]?.udid ?? null;
  drawTabs();
  if (!selected && Array.isArray(found)) {
    empty("Plug in an iPhone", "Connect it with a cable, unlock it, and tap Trust if it asks.");
  }
  if (selected && showing?.udid !== selected && failed !== selected) {
    start(phones.find((p) => p.udid === selected));
  }
}

function drawTabs() {
  const tabs = $("tabs");
  tabs.replaceChildren();
  if (phones.length < 2) return;
  for (const phone of phones) {
    const tab = document.createElement("button");
    tab.className = "tab" + (phone.udid === selected ? " selected" : "");
    tab.textContent = phone.name;
    tab.onclick = () => {
      selected = phone.udid;
      drawTabs();
      start(phone);
    };
    tabs.append(tab);
  }
}

function empty(title, text, problem = false) {
  failed = problem ? selected : null;
  canvas.hidden = true;
  $("empty").hidden = false;
  $("empty").classList.toggle("problem", problem);
  $("empty-title").textContent = title;
  $("empty-text").textContent = text;
  $("retry").hidden = !problem;
  status("");
}

function status(text) {
  $("status").textContent = text;
}

function start(phone, attempt = 1) {
  if (!phone) return;
  const major = parseInt(phone.ios ?? "0", 10);
  if (major && major < 27) {
    showing = null;
    return empty(phone.name, `Screen sharing needs iOS 27 or newer. This iPhone has iOS ${phone.ios}.`, true);
  }
  if (phone.developer_mode === false) {
    showing = null;
    return empty(phone.name, "Turn on Developer Mode on the iPhone: Settings › Privacy & Security › Developer Mode.", true);
  }
  const session = { udid: phone.udid };
  showing = session;
  resetDecoder();
  $("empty").hidden = true;
  canvas.hidden = false;
  canvas.width = 1290;
  canvas.height = 2796;
  context.fillStyle = "#000";
  context.fillRect(0, 0, canvas.width, canvas.height);
  status(phone.link === "wifi" ? "Connecting over Wi-Fi…" : "Connecting…");
  window.bridge
    .show(phone.udid, (bytes) => showing === session && decode(bytes))
    .then(() => {
      if (showing !== session) return;
      showing = null;
      empty(phone.name, "The screen stopped. Unlock the iPhone and try again.", true);
    })
    .catch((error) => {
      if (showing !== session) return;
      if (attempt === 1) {
        // the phone can still be closing a session that just ended
        status("Connecting again…");
        setTimeout(() => showing === session && start(phone, 2), 2000);
      } else {
        showing = null;
        empty(phone.name, String(error), true);
      }
    });
}

function resetDecoder() {
  if (decoder && decoder.state !== "closed") decoder.close();
  decoder = null;
  description = "";
}

// each message is a byte of 1 for a key frame then annex-b h.264, decode stays synchronous so no frame is skipped,
// a skipped frame breaks every frame after it
function decode(bytes) {
  const key = bytes[0] === 1;
  const data = bytes.subarray(1);
  if (!decoder && !key) return;
  if (key) {
    const config = { ...decoderConfig(data), optimizeForLatency: true };
    const setup = config.codec + String(config.description);
    if (!decoder) {
      decoder = new VideoDecoder({ output: draw, error: decodeFailed });
      status("");
    }
    if (config.description && setup !== description) {
      decoder.configure(config);
      description = setup;
    }
  }
  if (decoder.state !== "configured") return;
  decoder.decode(new EncodedVideoChunk({
    type: key ? "key" : "delta",
    timestamp: performance.now() * 1000,
    data: lengthPrefixed(data),
  }));
}

function draw(frame) {
  if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
    canvas.width = frame.displayWidth;
    canvas.height = frame.displayHeight;
  }
  context.drawImage(frame, 0, 0);
  frame.close();
  frameCount++;
}

// only a new stream sends a key frame, so start again once, a second failure soon after means it can't play at all
let lastRestart = 0;
function decodeFailed(error) {
  window.bridge.log(`decoder: ${error}`);
  const phone = phones.find((p) => p.udid === showing?.udid);
  showing = null;
  resetDecoder();
  window.bridge.stop();
  if (error.name === "NotSupportedError") {
    return empty("Can't play the video", "This computer can't decode H.264 video.", true);
  }
  if (performance.now() - lastRestart > 10000) {
    lastRestart = performance.now();
    start(phone);
  } else {
    empty("Can't play the video", `The video decoder stopped: ${error.message ?? error}`, true);
  }
}

// webkit only plays h.264 the way mp4 carries it, an avcC box and length-prefixed nal units, chromium takes it too
function decoderConfig(annexB) {
  const nals = nalUnits(annexB);
  const sps = nals.filter((nal) => nalType(nal) === 7);
  const pps = nals.filter((nal) => nalType(nal) === 8);
  if (!sps.length || !pps.length) return { codec: "avc1.640033" };
  const description = [1, sps[0][1], sps[0][2], sps[0][3], 0xff, 0xe0 | sps.length];
  for (const nal of sps) description.push(nal.length >> 8, nal.length & 0xff, ...nal);
  description.push(pps.length);
  for (const nal of pps) description.push(nal.length >> 8, nal.length & 0xff, ...nal);
  const codec = "avc1." + [sps[0][1], sps[0][2], sps[0][3]].map((b) => b.toString(16).padStart(2, "0")).join("");
  return { codec, description: Uint8Array.from(description) };
}

function lengthPrefixed(annexB) {
  const nals = nalUnits(annexB).filter((nal) => nalType(nal) !== 7 && nalType(nal) !== 8);
  const out = new Uint8Array(nals.reduce((sum, nal) => sum + 4 + nal.length, 0));
  const view = new DataView(out.buffer);
  let at = 0;
  for (const nal of nals) {
    view.setUint32(at, nal.length);
    out.set(nal, at + 4);
    at += 4 + nal.length;
  }
  return out;
}

const nalType = (nal) => nal[0] & 0x1f;

function nalUnits(data) {
  const starts = [];
  for (let i = 0; i + 2 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      starts.push(i + 3);
      i += 2;
    }
  }
  return starts.map((start, i) => {
    let end = i + 1 < starts.length ? starts[i + 1] - 3 : data.length;
    // a 4 byte start code leaves its first zero on the end of the nal before
    while (end > start && data[end - 1] === 0) end--;
    return data.subarray(start, end);
  });
}

// the iphone's screen corners, about an eighth of its width
new ResizeObserver(() => {
  canvas.style.borderRadius = `${canvas.getBoundingClientRect().width * 0.125}px`;
}).observe(canvas);

// ---- touches ----

const clamp = (v) => Math.round(Math.min(Math.max(v, 0), 1) * 65535);

function position(event) {
  const rect = canvas.getBoundingClientRect();
  return [clamp((event.clientX - rect.left) / rect.width), clamp((event.clientY - rect.top) / rect.height)];
}

let fingerDown = false;
let pendingMove = null;

canvas.addEventListener("pointerdown", (event) => {
  if (!decoder || event.button !== 0) return;
  canvas.setPointerCapture(event.pointerId);
  fingerDown = true;
  window.bridge.touch(...position(event), true);
});

canvas.addEventListener("pointermove", (event) => {
  if (!fingerDown) return;
  // one touch per screen refresh, more only queue up behind the phone
  if (!pendingMove) {
    requestAnimationFrame(() => {
      if (fingerDown && pendingMove) window.bridge.touch(...pendingMove, true);
      pendingMove = null;
    });
  }
  pendingMove = position(event);
});

function lift(event) {
  if (!fingerDown) return;
  fingerDown = false;
  pendingMove = null;
  window.bridge.touch(...position(event), false);
}
canvas.addEventListener("pointerup", lift);
canvas.addEventListener("pointercancel", lift);

// a mouse wheel or trackpad scroll becomes a finger dragging from the pointer, lifted once the scrolling stops so the
// phone carries on with its own momentum
let scroll = null;
canvas.addEventListener("wheel", (event) => {
  event.preventDefault();
  if (!decoder || fingerDown) return;
  const rect = canvas.getBoundingClientRect();
  const lines = event.deltaMode === 1 ? 16 : 1;
  if (!scroll) {
    const [x, y] = position(event);
    scroll = { x, y };
    window.bridge.touch(x, y, true);
  }
  scroll.x = clamp((scroll.x - event.deltaX * lines * (65535 / rect.width)) / 65535);
  scroll.y = clamp((scroll.y - event.deltaY * lines * (65535 / rect.height)) / 65535);
  window.bridge.touch(scroll.x, scroll.y, true);
  clearTimeout(scroll.timer);
  const ended = () => {
    window.bridge.touch(scroll.x, scroll.y, false);
    scroll = null;
  };
  // at the screen's edge the finger has nowhere left to go, start over from the pointer on the next scroll
  if (scroll.y === 0 || scroll.y === 65535 || scroll.x === 0 || scroll.x === 65535) ended();
  else scroll.timer = setTimeout(ended, 90);
}, { passive: false });

// ---- buttons, the bar and the window ----

const shortcut = mac ? "⌘" : "Ctrl+";
for (const button of document.querySelectorAll("[data-button]")) {
  button.onclick = () => window.bridge.press(button.dataset.button);
  if (button.dataset.shortcut) button.title += ` (${shortcut}${button.dataset.shortcut})`;
}
$("minimize").onclick = () => appWindow.minimize();
$("close").onclick = () => appWindow.close();

let sleepTimer = null;
function wake() {
  document.body.classList.add("awake");
  clearTimeout(sleepTimer);
  sleepTimer = setTimeout(() => document.body.classList.remove("awake"), 2500);
}
document.addEventListener("mousemove", wake);
document.addEventListener("mouseleave", () => {
  clearTimeout(sleepTimer);
  document.body.classList.remove("awake");
});

$("retry").onclick = () => {
  showing = null;
  failed = null;
  refreshPhones();
};

// ---- keyboard ----

// ctrl stands in for the iphone's command key on windows, so ctrl+c and ctrl+a do what they would on a pc, the
// windows key is left out so it still opens the start menu
const KEYS = {
  Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c,
  Minus: 0x2d, Equal: 0x2e, BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31,
  Semicolon: 0x33, Quote: 0x34, Backquote: 0x35, Comma: 0x36, Period: 0x37, Slash: 0x38, CapsLock: 0x39,
  Insert: 0x49, Home: 0x4a, PageUp: 0x4b, Delete: 0x4c, End: 0x4d, PageDown: 0x4e,
  ArrowRight: 0x4f, ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52,
  ControlLeft: 0xe3, ShiftLeft: 0xe1, AltLeft: 0xe2,
  ControlRight: 0xe7, ShiftRight: 0xe5, AltRight: 0xe6,
};
if (mac) Object.assign(KEYS, { ControlLeft: 0xe0, ControlRight: 0xe4, MetaLeft: 0xe3, MetaRight: 0xe7 });
for (let i = 0; i < 26; i++) KEYS["Key" + String.fromCharCode(65 + i)] = 0x04 + i;
for (let i = 1; i <= 9; i++) KEYS["Digit" + i] = 0x1d + i;
KEYS.Digit0 = 0x27;
const COMMAND = mac ? "metaKey" : "ctrlKey";

// let go of when the window loses focus so none stay stuck
const held = new Set();

const SHORTCUTS = {
  Digit1: () => window.bridge.press("home"),
  Digit2: () => window.bridge.press("app-switcher"),
  // command is already held, space completes command+space
  Digit3: () => window.bridge.keys([[0x2c, true], [0x2c, false]]),
};

window.addEventListener("keydown", (event) => {
  const usage = KEYS[event.code];
  if (!decoder || usage === undefined) return;
  if (event[COMMAND] && event.code === "KeyV") return;
  event.preventDefault();
  if (event[COMMAND] && SHORTCUTS[event.code]) {
    if (!event.repeat) SHORTCUTS[event.code]();
    return;
  }
  // the phone repeats a held key itself
  if (event.repeat) return;
  held.add(usage);
  window.bridge.keys([[usage, true]]);
});

window.addEventListener("keyup", (event) => {
  const usage = KEYS[event.code];
  if (usage === undefined || !held.has(usage)) return;
  event.preventDefault();
  held.delete(usage);
  window.bridge.keys([[usage, false]]);
});

window.addEventListener("blur", () => {
  window.bridge.keys([...held].map((usage) => [usage, false]));
  held.clear();
});

// characters to hid usages on a us layout, with whether they need shift
const TYPED = {};
"abcdefghijklmnopqrstuvwxyz".split("").forEach((c, i) => {
  TYPED[c] = [0x04 + i, false];
  TYPED[c.toUpperCase()] = [0x04 + i, true];
});
"1234567890".split("").forEach((c, i) => (TYPED[c] = [0x1e + i, false]));
"!@#$%^&*()".split("").forEach((c, i) => (TYPED[c] = [0x1e + i, true]));
for (const [plain, shifted, usage] of [
  ["-", "_", 0x2d], ["=", "+", 0x2e], ["[", "{", 0x2f], ["]", "}", 0x30], ["\\", "|", 0x31],
  [";", ":", 0x33], ["'", '"', 0x34], ["`", "~", 0x35], [",", "<", 0x36], [".", ">", 0x37], ["/", "?", 0x38],
]) {
  TYPED[plain] = [usage, false];
  TYPED[shifted] = [usage, true];
}
TYPED[" "] = [0x2c, false];
TYPED["\n"] = [0x28, false];
TYPED["\t"] = [0x2b, false];

// pasting types the text out on the phone, characters a us keyboard can't type, like emoji, are skipped, one call so
// the keys can't arrive out of order
window.addEventListener("paste", (event) => {
  if (!decoder) return;
  event.preventDefault();
  const text = (event.clipboardData?.getData("text") ?? "").replace(/\r\n?/g, "\n");
  const presses = [];
  // command is still held from ctrl+v, let go of it or the phone sees command+each letter
  for (const usage of held) presses.push([usage, false]);
  held.clear();
  for (const char of text) {
    const typed = TYPED[char];
    if (!typed) continue;
    const [usage, shift] = typed;
    if (shift) presses.push([0xe1, true]);
    presses.push([usage, true], [usage, false]);
    if (shift) presses.push([0xe1, false]);
  }
  window.bridge.keys(presses);
});

// the frame rate only shows when it drops
setInterval(() => {
  const now = performance.now();
  const fps = frameCount / ((now - fpsSince) / 1000);
  frameCount = 0;
  fpsSince = now;
  if (decoder) status(fps < 20 ? `${Math.round(fps)} fps` : "");
}, 2000);

refreshPhones();
setInterval(() => {
  if (!showing) refreshPhones();
}, 3000);
