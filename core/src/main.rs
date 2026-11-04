//! controlmyiphone-cli, the engine on its own, for testing without the app
//!
//!   controlmyiphone-cli list
//!   controlmyiphone-cli stream <udid> [seconds] > screen.h264
//!   controlmyiphone-cli tap <udid> <x> <y>        x and y from 0 to 65535 across the screen
//!   controlmyiphone-cli button <udid> home|lock|volume-up|volume-down

use std::{
    io::Write,
    time::{Duration, Instant},
};

use controlmyiphone_core::{self as engine, Button, Frame, Input};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["list"] => list().await,
        ["stream", udid] => stream(udid, None).await,
        ["stream", udid, seconds] => stream(udid, seconds.parse().ok()).await,
        ["tap", udid, x, y] => match (x.parse(), y.parse()) {
            (Ok(x), Ok(y)) => press(udid, &[Input::Touch { x, y }, Input::Lift { x, y }]).await,
            _ => Err(engine::Error::Other("x and y run from 0 to 65535".into())),
        },
        ["button", udid, name] => match button(name) {
            Some(b) => press(udid, &[Input::Button(b)]).await,
            None => Err(engine::Error::Other(
                "buttons: home, lock, volume-up, volume-down".into(),
            )),
        },
        _ => {
            eprintln!(
                "usage: controlmyiphone-cli list | stream <udid> [seconds] | tap <udid> <x> <y> | button <udid> <name>"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn list() -> engine::Result<()> {
    for phone in engine::phones().await? {
        let link = match phone.link {
            engine::Link::Usb => "usb",
            engine::Link::WiFi => "wifi",
        };
        let ios = phone.ios.as_deref().unwrap_or("?");
        let developer = match phone.developer_mode {
            Some(true) => "on",
            Some(false) => "off",
            None => "?",
        };
        println!(
            "{}\t{}\t{link}\tios {ios}\tdeveloper mode {developer}",
            phone.udid, phone.name
        );
    }
    Ok(())
}

/// writes the screen to stdout as an h.264 stream ffplay or vlc can play, with a frame count on stderr
async fn stream(udid: &str, seconds: Option<u64>) -> engine::Result<()> {
    let (_inputs, received) = mpsc::unbounded_channel();
    let mut out = std::io::BufWriter::new(std::io::stdout());
    let started = Instant::now();
    let mut frames = 0u64;
    let mut bytes = 0usize;
    let show = |frame: Frame| {
        frames += 1;
        bytes += frame.data.len();
        if frames % 60 == 0 {
            let secs = started.elapsed().as_secs_f64();
            eprintln!(
                "{frames} frames, {:.1} fps, {:.1} Mbit/s",
                frames as f64 / secs,
                bytes as f64 * 8.0 / secs / 1e6
            );
        }
        // a closed pipe means the player went away
        out.write_all(&frame.data).and_then(|_| out.flush()).is_ok()
            && seconds.is_none_or(|s| started.elapsed() < Duration::from_secs(s))
    };
    engine::mirror(udid, show, received).await
}

/// sends the inputs once the screen is showing, the phone ignores them before that
async fn press(udid: &str, inputs: &[Input]) -> engine::Result<()> {
    let (sender, received) = mpsc::unbounded_channel();
    let mut frames = 0;
    let inputs = inputs.to_vec();
    let show = move |_: Frame| {
        frames += 1;
        if frames == 30 {
            for input in &inputs {
                let _ = sender.send(*input);
            }
        }
        frames < 120
    };
    engine::mirror(udid, show, received).await
}

fn button(name: &str) -> Option<Button> {
    Some(match name {
        "home" => Button::Home,
        "lock" => Button::Lock,
        "volume-up" => Button::VolumeUp,
        "volume-down" => Button::VolumeDown,
        _ => return None,
    })
}
