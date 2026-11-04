# controlmyiphone: your iPhone's screen on your PC

**See your iPhone's screen live in a window on your Windows PC, for free.** Plug it in and it shows up at up to 60 frames per second, full resolution. Click and drag on it to tap and swipe. You don't need a jailbreak or an app on the phone.

<p>
  <a href="https://github.com/meekwilliam/controlmyiphone/releases/latest/download/controlmyiphone.exe"><img alt="Download for Windows" src="https://img.shields.io/badge/Download_for-Windows-1f6feb?style=for-the-badge"></a>
</p>

[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS-lightgrey)
![iOS](https://img.shields.io/badge/iOS-27%2B-lightgrey)

## Requirements

- An iPhone on **iOS 27 or newer**, with **Developer Mode** on (Settings › Privacy & Security › Developer Mode).
- **Windows 10 or 11** with [**Apple Devices**](https://apps.microsoft.com/detail/9np83lwlpz9k) from the Microsoft Store, which gives Windows the iPhone driver.

## Install

[Download `controlmyiphone.exe`](https://github.com/meekwilliam/controlmyiphone/releases/latest/download/controlmyiphone.exe) and run it. SmartScreen may warn that it's unrecognized, because it isn't code-signed. Choose **More info › Run anyway**.

## Use

Plug in your iPhone, unlock it, and tap **Trust** if it asks. Its screen appears in the window. Click to tap, drag to swipe, and use the buttons under the screen for Home, Lock and volume.

## How it works

It uses the same screen sharing as Apple's Xcode 27: the iPhone sends its screen as video through Apple's developer connection, and takes touches back the same way. The engine is written in Rust on [idevice](https://github.com/jkcoxson/idevice).

## License

MIT
