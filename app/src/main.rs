// no console window behind the app in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! controlmyiphone's window, the page in web/ plus these commands, which call the engine in core/, the page decodes
//! the video itself with the computer's own h.264 decoder, so the frames go to it untouched

use std::sync::Mutex;

use controlmyiphone_core::{self as engine, Button, Frame, Input};
use serde_json::{Value, json};
use tauri::{
    State,
    ipc::{Channel, InvokeResponseBody},
};
use tokio::sync::{mpsc, oneshot};

/// where the page's clicks go, the screen being shown right now, none while nothing is
#[derive(Default)]
struct Showing(Mutex<Option<mpsc::UnboundedSender<Input>>>);

#[tauri::command]
async fn phones() -> Value {
    match engine::phones().await {
        Ok(found) => json!(found),
        Err(_) => {
            json!({ "error": "Can't see any iPhones. Install Apple Devices from the Microsoft Store, then plug in your iPhone." })
        }
    }
}

/// streams the phone's screen to the page until `stop`, another `show`, or the page going away, each message is
/// one frame, a first byte of 1 for a key frame, then the h.264 data
#[tauri::command]
async fn show(udid: String, frames: Channel<InvokeResponseBody>, showing: State<'_, Showing>) -> Result<(), String> {
    let (sender, inputs) = mpsc::unbounded_channel();
    // replacing the old sender ends the screen shown before
    *showing.0.lock().unwrap() = Some(sender);
    let send = move |frame: Frame| {
        let mut message = Vec::with_capacity(frame.data.len() + 1);
        message.push(frame.key as u8);
        message.extend_from_slice(&frame.data);
        frames.send(InvokeResponseBody::Raw(message)).is_ok()
    };
    // the engine's future isn't Send, so it gets a thread and runtime of its own, which also keeps the video off
    // the threads the window's other commands run on
    let (done, finished) = oneshot::channel();
    std::thread::spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime
                .block_on(engine::mirror(&udid, send, inputs))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = done.send(result);
    });
    finished
        .await
        .unwrap_or_else(|_| Err("The screen stopped unexpectedly.".into()))
}

#[tauri::command]
fn stop(showing: State<'_, Showing>) {
    showing.0.lock().unwrap().take();
}

/// a finger at x, y, from 0 to 65535 across the screen, `down` false lifts it
#[tauri::command]
fn touch(x: u16, y: u16, down: bool, showing: State<'_, Showing>) {
    let input = if down {
        Input::Touch { x, y }
    } else {
        Input::Lift { x, y }
    };
    if let Some(sender) = showing.0.lock().unwrap().as_ref() {
        let _ = sender.send(input);
    }
}

#[tauri::command]
fn press(button: String, showing: State<'_, Showing>) {
    let button = match button.as_str() {
        "home" => Button::Home,
        "lock" => Button::Lock,
        "volume-up" => Button::VolumeUp,
        "volume-down" => Button::VolumeDown,
        _ => return,
    };
    if let Some(sender) = showing.0.lock().unwrap().as_ref() {
        let _ = sender.send(Input::Button(button));
    }
}

/// keyboard keys going down or up, by their hid keyboard usage, in order, a whole pasted text comes as one call
#[tauri::command]
fn keys(presses: Vec<(u8, bool)>, showing: State<'_, Showing>) {
    if let Some(sender) = showing.0.lock().unwrap().as_ref() {
        for (usage, down) in presses {
            let _ = sender.send(Input::Key { usage, down });
        }
    }
}

/// the page's errors, for whoever runs the app from a terminal
#[tauri::command]
fn log(text: String) {
    eprintln!("page: {text}");
}

/// only ever opens controlmyiphone's own github pages
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

fn main() {
    tauri::Builder::default()
        .manage(Showing::default())
        .invoke_handler(tauri::generate_handler![
            phones, show, stop, touch, press, keys, open_page, log
        ])
        .run(tauri::generate_context!())
        .expect("couldn't start controlmyiphone");
}
