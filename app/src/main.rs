// no console window behind the app in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{sync::Mutex, thread::JoinHandle, time::Duration};

use controlmyiphone_core::{self as engine, Button, Frame, Input};
use serde_json::{Value, json};
use tauri::{
    AppHandle, Manager, RunEvent, State, WindowEvent,
    ipc::{Channel, InvokeResponseBody},
};
use tokio::sync::{mpsc, oneshot};

#[derive(Default)]
struct Showing {
    inputs: Mutex<Option<mpsc::UnboundedSender<Input>>>,
    streams: Mutex<Vec<JoinHandle<()>>>,
}

impl Showing {
    fn send(&self, input: Input) {
        if let Some(sender) = self.inputs.lock().unwrap().as_ref() {
            let _ = sender.send(input);
        }
    }
}

#[tauri::command]
async fn phones() -> Value {
    match engine::phones().await {
        Ok(found) => json!(found),
        Err(_) => {
            json!({ "error": "Can't see any iPhones. Install Apple Devices from the Microsoft Store, then plug in your iPhone." })
        }
    }
}

// each message is a byte of 1 for a key frame, then the frame's annex-b h.264
#[tauri::command]
async fn show(udid: String, frames: Channel<InvokeResponseBody>, showing: State<'_, Showing>) -> Result<(), String> {
    let (sender, inputs) = mpsc::unbounded_channel();
    // replacing the old sender ends the screen shown before
    *showing.inputs.lock().unwrap() = Some(sender);
    let send = move |frame: Frame| {
        let mut message = Vec::with_capacity(frame.data.len() + 1);
        message.push(frame.key as u8);
        message.extend_from_slice(&frame.data);
        frames.send(InvokeResponseBody::Raw(message)).is_ok()
    };
    // the engine's future isn't Send, so it gets a thread and runtime of its own, which also keeps the video off
    // the threads the window's other commands run on
    let (done, finished) = oneshot::channel();
    let stream = std::thread::spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime
                .block_on(engine::mirror(&udid, send, inputs))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = done.send(result);
    });
    showing.streams.lock().unwrap().push(stream);
    finished
        .await
        .unwrap_or_else(|_| Err("The screen stopped unexpectedly.".into()))
}

#[tauri::command]
fn stop(showing: State<'_, Showing>) {
    showing.inputs.lock().unwrap().take();
}

#[tauri::command]
fn touch(x: u16, y: u16, down: bool, showing: State<'_, Showing>) {
    let input = if down {
        Input::Touch { x, y }
    } else {
        Input::Lift { x, y }
    };
    showing.send(input);
}

#[tauri::command]
fn press(button: String, showing: State<'_, Showing>) {
    let button = match button.as_str() {
        "home" => Button::Home,
        "app-switcher" => Button::AppSwitcher,
        "lock" => Button::Lock,
        "volume-up" => Button::VolumeUp,
        "volume-down" => Button::VolumeDown,
        _ => return,
    };
    showing.send(Input::Button(button));
}

#[tauri::command]
fn keys(presses: Vec<(u8, bool)>, showing: State<'_, Showing>) {
    for (usage, down) in presses {
        showing.send(Input::Key { usage, down });
    }
}

#[tauri::command]
fn log(text: String) {
    eprintln!("page: {text}");
}

#[tauri::command]
fn open_page(url: String) {
    if !url.starts_with("https://github.com/meekwilliam/controlmyiphone") {
        return;
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(&url).spawn();
    #[cfg(not(windows))]
    let _ = std::process::Command::new("open").arg(&url).spawn();
}

/// tells the phone to stop sharing before quitting, returns false when nothing was being shown
fn stop_then_exit(app: &AppHandle) -> bool {
    let showing = app.state::<Showing>();
    let streams = std::mem::take(&mut *showing.streams.lock().unwrap());
    if streams.is_empty() {
        return false;
    }
    showing.inputs.lock().unwrap().take();
    for window in app.webview_windows().values() {
        let _ = window.hide();
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let (stopped, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in streams {
                let _ = stream.join();
            }
            let _ = stopped.send(());
        });
        // a phone that stopped answering mustn't keep the app open
        let _ = wait.recv_timeout(Duration::from_secs(3));
        app.exit(0);
    });
    true
}

fn main() {
    tauri::Builder::default()
        .manage(Showing::default())
        // just quitting leaves the phone sharing its screen, with the recording indicator on, for up to an hour
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && stop_then_exit(window.app_handle())
            {
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            phones, show, stop, touch, press, keys, open_page, log
        ])
        .build(tauri::generate_context!())
        .expect("couldn't start controlmyiphone")
        .run(|app, event| {
            if let RunEvent::ExitRequested { api, .. } = event
                && stop_then_exit(app)
            {
                api.prevent_exit();
            }
        });
}
