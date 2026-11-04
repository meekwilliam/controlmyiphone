const { invoke, Channel } = window.__TAURI__.core;

window.bridge = {
  phones: () => invoke("phones"),
  show: (udid, onFrame) => {
    const frames = new Channel();
    frames.onmessage = (data) => onFrame(new Uint8Array(data));
    return invoke("show", { udid, frames });
  },
  stop: () => invoke("stop"),
  touch: (x, y, down) => invoke("touch", { x, y, down }),
  press: (button) => invoke("press", { button }),
  keys: (presses) => invoke("keys", { presses }),
  open: (url) => invoke("open_page", { url }),
  log: (text) => invoke("log", { text }),
};

// page errors end up in the app's own output, the window has no console of its own in release builds
window.addEventListener("error", (event) => window.bridge.log(String(event.message)));
window.addEventListener("unhandledrejection", (event) => window.bridge.log(String(event.reason)));
