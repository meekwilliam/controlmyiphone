// the page, finds iphones, shows the chosen one's screen and turns mouse clicks into touches on it

const $ = (id) => document.getElementById(id);
const canvas = $("video");
const context = canvas.getContext("2d");

let phones = [];
let selected = null;
// the phone whose screen is coming in right now, a new object per stream so an old stream ending can't touch a
// newer one of the same phone
let showing = null;
// a phone whose screen just failed, not started again on its own until try again
let failed = null;
let decoder = null;
let frameCount = 0;
let fpsSince = performance.now();

// ---- phones ----

async function refreshPhones() {
  const found = await window.bridge.phones();
  phones = Array.isArray(found) ? found : [];
  if (!Array.isArray(found)) {
    empty("No iPhone found", found.error, true);
  }
  // a phone that was unplugged gets a fresh start when it's back
  if (!phones.some((p) => p.udid === failed)) failed = null;
  if (!phones.some((p) => p.udid === selected)) {
    selected = phones[0]?.udid ?? null;
  }
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
  $("screen").hidden = true;
  $("buttons").hidden = true;
  $("empty").hidden = false;
  $("empty").classList.toggle("problem", problem);
  $("empty-title").textContent = title;
  $("empty-text").textContent = text;
  $("retry").hidden = !problem;
}

// ---- the screen ----

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
  const udid = phone.udid;
  const session = { udid };
  showing = session;
  resetDecoder();
  $("empty").hidden = true;
  $("screen").hidden = false;
  $("buttons").hidden = false;
  canvas.width = 390;
  canvas.height = 844;
  context.fillStyle = "#000";
  context.fillRect(0, 0, canvas.width, canvas.height);
  status(phone.link === "wifi" ? "Connecting over Wi-Fi…" : "Connecting…");
  window.bridge
    .show(udid, (bytes) => showing === session && decode(bytes))
    .then(() => {
      if (showing === session) {
        showing = null;
        empty(phone.name, "The screen stopped. Unlock the iPhone and try again.", true);
      }
    })
    .catch((error) => {
      if (showing === session && attempt === 1) {
        // the phone can still be closing a screen session that just ended, give it a moment
        status("Connecting again…");
        setTimeout(() => showing === session && start(phone, 2), 2000);
      } else if (showing === session) {
        showing = null;
        empty(phone.name, String(error), true);
      }
    });
}

function status(text) {
  $("status").textContent = text;
}

function resetDecoder() {
  if (decoder && decoder.state !== "closed") decoder.close();
  decoder = null;
  description = "";
}

// each message is a first byte of 1 for a key frame, then annex-b h.264
// what the last frame held, for the log when the decoder fails
let lastTypes = "";
// the setup the decoder has now, the phone can start over with new settings at any key frame
let description = "";
// stays synchronous so frames reach the decoder in order, any frame skipped breaks every frame after it
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
    // a key frame without its setup (none seen yet) can't configure anything
    if (config.description && setup !== description) {
      // a codec this computer can't play shows up as a NotSupportedError in decodeFailed
      decoder.configure(config);
      description = setup;
      window.bridge.log(`decoding ${config.codec}, ${config.description?.length} byte setup`);
    }
  }
  if (decoder.state !== "configured") return;
  lastTypes = nalUnits(data).map(nalType).join(",") + ` ${data.length}B key=${key}`;
  const chunk = lengthPrefixed(data);
  decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: performance.now() * 1000, data: chunk }));
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

// a broken frame breaks every frame after it until the next key frame, which only a new stream sends, so start
// again, once, a second failure soon after means the decoder can't play this video at all
let lastRestart = 0;
function decodeFailed(error) {
  window.bridge.log(`decoder: ${error}, last frame ${lastTypes}`);
  const phone = phones.find((p) => p.udid === showing?.udid);
  showing = null;
  resetDecoder();
  window.bridge.stop();
  if (error.name === "NotSupportedError") {
    return empty(
      "Can't play the video",
      "This computer can't decode H.264 video.",
      true,
    );
  }
  if (performance.now() - lastRestart > 10000) {
    lastRestart = performance.now();
    start(phone);
  } else {
    empty("Can't play the video", `The video decoder stopped: ${error.message ?? error}`, true);
  }
}

// the decoder gets h.264 set up the way mp4 files carry it, an avcC box with the sps and pps and frames with a length
// before each nal unit instead of start codes, webkit only plays it this way, chromium takes it too
function decoderConfig(annexB) {
  const nals = nalUnits(annexB);
  const sps = nals.filter((nal) => nalType(nal) === 7);
  const pps = nals.filter((nal) => nalType(nal) === 8);
  if (!sps.length || !pps.length) return { codec: "avc1.640033" };
  const description = [1, sps[0][1], sps[0][2], sps[0][3], 0xff, 0xe0 | sps.length];
  for (const nal of sps) description.push(nal.length >> 8, nal.length & 0xff, ...nal);
  description.push(pps.length);
  for (const nal of pps) description.push(nal.length >> 8, nal.length & 0xff, ...nal);
  // profile, constraint flags and level, like avc1.640033
  const codec = "avc1." + [sps[0][1], sps[0][2], sps[0][3]].map((b) => b.toString(16).padStart(2, "0")).join("");
  return { codec, description: Uint8Array.from(description) };
}

// a frame's nal units each behind a 4 byte length, the sps and pps already went in the description
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

// ---- touches ----

let fingerDown = false;
let pendingMove = null;

function position(event) {
  const rect = canvas.getBoundingClientRect();
  const clamp = (v) => Math.round(Math.min(Math.max(v, 0), 1) * 65535);
  return [clamp((event.clientX - rect.left) / rect.width), clamp((event.clientY - rect.top) / rect.height)];
}

canvas.addEventListener("pointerdown", (event) => {
  if (!decoder) return;
  canvas.setPointerCapture(event.pointerId);
  fingerDown = true;
  window.bridge.touch(...position(event), true);
});

canvas.addEventListener("pointermove", (event) => {
  if (!fingerDown) return;
  // one touch per screen refresh is plenty, more only queues up behind the phone
  if (!pendingMove) requestAnimationFrame(() => {
    if (fingerDown && pendingMove) window.bridge.touch(...pendingMove, true);
    pendingMove = null;
  });
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

for (const button of document.querySelectorAll("[data-button]")) {
  button.onclick = () => window.bridge.press(button.dataset.button);
}

// ---- keyboard ----

// keys by their position, to hid keyboard usages, on windows ctrl stands in for the iphone's command key so ctrl+c and
// ctrl+a do what they would on a pc, and the windows key is left out so it still opens the start menu
const KEYS = {
  Enter: 0x28, Escape: 0x29, Backspace: 0x2a, Tab: 0x2b, Space: 0x2c,
  Minus: 0x2d, Equal: 0x2e, BracketLeft: 0x2f, BracketRight: 0x30, Backslash: 0x31,
  Semicolon: 0x33, Quote: 0x34, Backquote: 0x35, Comma: 0x36, Period: 0x37, Slash: 0x38, CapsLock: 0x39,
  Insert: 0x49, Home: 0x4a, PageUp: 0x4b, Delete: 0x4c, End: 0x4d, PageDown: 0x4e,
  ArrowRight: 0x4f, ArrowLeft: 0x50, ArrowDown: 0x51, ArrowUp: 0x52,
  ControlLeft: 0xe3, ShiftLeft: 0xe1, AltLeft: 0xe2,
  ControlRight: 0xe7, ShiftRight: 0xe5, AltRight: 0xe6,
};
if (navigator.platform.startsWith("Mac")) {
  Object.assign(KEYS, { ControlLeft: 0xe0, ControlRight: 0xe4, MetaLeft: 0xe3, MetaRight: 0xe7 });
}
const PASTE = navigator.platform.startsWith("Mac") ? "metaKey" : "ctrlKey";
for (let i = 0; i < 26; i++) KEYS["Key" + String.fromCharCode(65 + i)] = 0x04 + i;
for (let i = 1; i <= 9; i++) KEYS["Digit" + i] = 0x1d + i;
KEYS.Digit0 = 0x27;

// keys down on the phone right now, let go of when the window loses focus so none stay stuck
const held = new Set();

window.addEventListener("keydown", (event) => {
  const usage = KEYS[event.code];
  if (!decoder || usage === undefined) return;
  // ctrl+v pastes this computer's clipboard instead, see the paste handler
  if (event.code === "KeyV" && event[PASTE]) return;
  event.preventDefault();
  // the phone repeats a held key by itself
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

// ctrl+v, or cmd+v on a mac, types out this computer's clipboard on the phone, a key at a time, characters a us keyboard can't type,
// like emoji, are skipped
window.addEventListener("paste", (event) => {
  if (!decoder) return;
  event.preventDefault();
  const text = (event.clipboardData?.getData("text") ?? "").replace(/\r\n?/g, "\n");
  const presses = [];
  for (const char of text) {
    const typed = TYPED[char];
    if (!typed) continue;
    const [usage, shift] = typed;
    if (shift) presses.push([0xe1, true]);
    presses.push([usage, true], [usage, false]);
    if (shift) presses.push([0xe1, false]);
  }
  // one call for the whole text so the keys can't arrive out of order
  window.bridge.keys(presses);
});

$("retry").onclick = () => {
  showing = null;
  failed = null;
  refreshPhones();
};

// ---- frame rate, shown only when it drops ----

setInterval(() => {
  const now = performance.now();
  const fps = frameCount / ((now - fpsSince) / 1000);
  frameCount = 0;
  fpsSince = now;
  if (decoder && fps < 20) status(`${Math.round(fps)} fps`);
  else if (decoder) status("");
}, 2000);

refreshPhones();
setInterval(() => {
  if (!showing) refreshPhones();
}, 3000);
