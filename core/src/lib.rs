use std::{
    fmt,
    future::Future,
    path::PathBuf,
    time::{Duration, Instant},
};

use idevice::{
    IdeviceError, IdeviceService, ReadWrite, RsdService,
    core_device::{
        ButtonState, CallInfoBlob, DisplayServiceClient, IndigoHidClient, RtpPacket, TOUCHSCREEN_STATE_CONTACT,
        TOUCHSCREEN_STATE_RELEASE, UniversalHidServiceClient, build_keyframe_request, build_liveness,
        build_screen_audio_offer, build_start_audio_parameters, build_start_video_parameters, is_rtcp,
    },
    core_device_proxy::CoreDeviceProxy,
    lockdown::LockdownClient,
    mobile_image_mounter::ImageMounter,
    provider::{IdeviceProvider, RsdProvider},
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
    xpc::{Dictionary as XpcDictionary, XPCObject},
};
use serde::Serialize;
use tokio::sync::mpsc;

mod h264;

const LABEL: &str = "controlmyiphone";

// ios 26 accepts the request but never sends a frame
const FIRST_IOS: u64 = 27;

const DDI_URL: &str =
    "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Personalized";
const DDI_FILES: [&str; 3] = ["Image.dmg", "Image.dmg.trustcache", "BuildManifest.plist"];

// device hub sends 140, the phone's own bigger mask gets an unusable video setup
const CLIENT_FEATURES: u64 = 140;

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Device(IdeviceError),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(udid) => write!(f, "Can't reach the iPhone {udid}. Unlock it, or plug it in."),
            Error::Device(IdeviceError::DeviceLocked) => f.write_str(
                "Unlock the iPhone. After a restart, it has to be unlocked once to load its developer tools.",
            ),
            Error::Device(IdeviceError::DeveloperModeNotEnabled) => {
                f.write_str("Turn on Developer Mode on the iPhone: Settings › Privacy & Security › Developer Mode.")
            }
            Error::Device(IdeviceError::InvalidHostID) => {
                f.write_str("This iPhone doesn't trust this computer yet. Plug it in, unlock it, and tap Trust.")
            }
            Error::Device(IdeviceError::Timeout) => {
                f.write_str("The iPhone stopped answering. Unlock it, or plug it in.")
            }
            Error::Device(e) => write!(f, "{e}"),
            Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

impl From<IdeviceError> for Error {
    fn from(e: IdeviceError) -> Self {
        Error::Device(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Other(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Link {
    Usb,
    WiFi,
}

#[derive(Debug, Clone, Serialize)]
pub struct Phone {
    pub udid: String,
    pub name: String,
    pub link: Link,
    pub ios: Option<String>,
    pub developer_mode: Option<bool>,
}

pub struct Frame {
    pub data: Vec<u8>,
    pub key: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Input {
    Touch { x: u16, y: u16 },
    Lift { x: u16, y: u16 },
    Button(Button),
    Key { usage: u8, down: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Home,
    AppSwitcher,
    Lock,
    VolumeUp,
    VolumeDown,
}

impl Button {
    // usage page, usage, hold ms, presses
    fn usage(self) -> (u64, u64, u64, usize) {
        match self {
            Button::Home => (0x0C, 0x40, 80, 1),
            Button::AppSwitcher => (0x0C, 0x40, 60, 2),
            Button::Lock => (0x0C, 0x30, 200, 1),
            Button::VolumeUp => (0x0C, 0xE9, 80, 1),
            Button::VolumeDown => (0x0C, 0xEA, 80, 1),
        }
    }
}

pub async fn phones() -> Result<Vec<Phone>> {
    let mut found: Vec<Phone> = Vec::new();
    for device in devices().await? {
        let link = match device.connection_type {
            Connection::Usb => Link::Usb,
            Connection::Network(_) => Link::WiFi,
            Connection::Unknown(_) => continue,
        };
        if let Some(phone) = found.iter_mut().find(|p| p.udid == device.udid) {
            if link == Link::Usb {
                phone.link = Link::Usb;
            }
            continue;
        }
        let provider = device.to_provider(UsbmuxdAddr::default(), LABEL);
        let name = lockdown_string(&provider, "DeviceName")
            .await
            .unwrap_or_else(|| format!("iPhone {}", short_udid(&device.udid)));
        let ios = lockdown_string(&provider, "ProductVersion").await;
        let developer_mode = developer_mode(&provider).await;
        found.push(Phone {
            udid: device.udid,
            name,
            link,
            ios,
            developer_mode,
        });
    }
    found.sort_by_key(|p| p.name.to_lowercase());
    Ok(found)
}

pub async fn mirror(
    udid: &str,
    mut show: impl FnMut(Frame) -> bool,
    mut inputs: mpsc::UnboundedReceiver<Input>,
) -> Result<()> {
    let provider = provider(udid).await?;
    if let Some(ios) = lockdown_string(&provider, "ProductVersion").await {
        let major = ios.split('.').next().and_then(|m| m.parse::<u64>().ok()).unwrap_or(0);
        if major < FIRST_IOS {
            return Err(Error::Other(format!(
                "Screen sharing needs iOS {FIRST_IOS} or newer. This iPhone has iOS {ios}."
            )));
        }
    }

    let loaded = load_developer_tools(&provider).await?;
    let (mut adapter, mut handshake) = tunnel(&provider).await?;
    if !loaded {
        load_developer_tools_tunneled(&provider, &mut adapter, &mut handshake).await?;
    }

    let mut screen = Screen::start(&mut adapter, &mut handshake).await?;
    let mut controls = Controls::default();
    let mut depacketizer = h264::Depacketizer::default();
    let mut feedback = Feedback::new(screen.ssrc);
    let mut alive = tokio::time::interval(Duration::from_secs(1));
    let mut expected: Option<u16> = None;
    // after a lost packet every frame is broken until the next key frame, so none are shown until then
    let mut broken = false;
    let mut setup = Setup::default();

    let result = loop {
        tokio::select! {
            datagram = screen.video.recv() => {
                let datagram = match datagram {
                    Ok(d) => d,
                    Err(e) => break Err(e.into()),
                };
                // the phone's rtcp timing reports come in on the same port, they'd read as broken video packets
                if is_rtcp(&datagram.data) {
                    continue;
                }
                let Some(packet) = RtpPacket::parse(&datagram.data) else { continue };
                feedback.phone = Some((datagram.source_port, packet.ssrc));
                if let Some(expected) = expected
                    && packet.sequence_number.wrapping_sub(expected) < 0x8000
                    && packet.sequence_number != expected
                {
                    eprintln!("Lost {} video packets, asking the iPhone for a fresh frame.", packet.sequence_number.wrapping_sub(expected));
                    broken = true;
                    // the depacketizer would wait for the lost packet, the tunnel keeps order so it never comes
                    depacketizer = h264::Depacketizer::default();
                    feedback.ask_for_key_frame(&screen.video).await;
                }
                if expected.is_none_or(|e| packet.sequence_number.wrapping_sub(e) < 0x8000) {
                    expected = Some(packet.sequence_number.wrapping_add(1));
                }
                depacketizer.push(packet.payload);
                // the marker bit closes a picture, everything before it belongs together
                if !packet.marker {
                    continue;
                }
                let mut data = depacketizer.take();
                if data.is_empty() {
                    continue;
                }
                let key = h264::has_key_frame(&data);
                if key {
                    data = setup.attach(data);
                }
                if broken && !key {
                    continue;
                }
                broken = false;
                if !show(Frame { data, key }) {
                    break Ok(());
                }
            }
            _ = alive.tick() => feedback.still_watching(&screen.video).await,
            input = inputs.recv() => match input {
                Some(input) => {
                    if let Err(e) = controls.send(input, &mut adapter, &mut handshake).await {
                        eprintln!("Couldn't pass {input:?} to the iPhone: {e}");
                    }
                }
                None => break Ok(()),
            },
        }
    };
    let _ = screen.client.stop_media_stream().await;
    result
}

// idevice asks for 3600, which keeps a killed app's session, and the phone's recording indicator, going for an hour,
// a live session stays up past this because feedback reaches the phone every second
fn with_timeout(mut params: XpcDictionary) -> XpcDictionary {
    params.insert("timeout".into(), XPCObject::UInt64(15));
    params
}

/// ends screen sharing left running on the phone, like by an app that was killed
pub async fn stop_sharing(udid: &str) -> Result<()> {
    let provider = provider(udid).await?;
    let (mut adapter, mut handshake) = tunnel(&provider).await?;
    let mut client = DisplayServiceClient::connect_rsd(&mut adapter, &mut handshake).await?;
    client.stop_media_stream().await?;
    Ok(())
}

async fn tunnel(provider: &dyn IdeviceProvider) -> Result<(AdapterHandle, RsdHandshake)> {
    let proxy = CoreDeviceProxy::connect(provider).await?;
    let rsd_port = proxy.tunnel_info().server_rsd_port;
    let mut adapter = proxy.create_software_tunnel()?.to_async_handle();
    let stream = adapter.connect(rsd_port).await?;
    let handshake = RsdHandshake::new(stream).await?;
    Ok((adapter, handshake))
}

pub async fn within_a_minute<T>(action: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(60), action)
        .await
        .unwrap_or(Err(Error::Device(IdeviceError::Timeout)))
}

pub fn short_udid(udid: &str) -> &str {
    &udid[udid.len().saturating_sub(4)..]
}

// the sockets have to stay open or the phone stops sending
struct Screen {
    client: DisplayServiceClient<Box<dyn ReadWrite>>,
    _audio: UdpSocketHandle,
    video: UdpSocketHandle,
    ssrc: u32,
}

impl Screen {
    // like device hub, audio first to set up the session, video fails without it
    async fn start(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Self> {
        let mut client = DisplayServiceClient::connect_rsd(adapter, handshake).await?;
        // a session a killed app left behind keeps the phone's recording indicator on for up to an hour
        let _ = client.stop_media_stream().await;
        let audio = adapter.bind_udp(0).await?;
        let video = adapter.bind_udp(0).await?;
        let host = adapter.host_ip().to_string();
        let phone = adapter.peer_ip().to_string();

        // the phone checks this looks like a mac running device hub, these are the values a captured offer had
        let call_info = CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        };
        let session = uuid::Uuid::new_v4();
        let call_id = || uuid::Uuid::new_v4().to_string().to_uppercase();

        let offer = build_screen_audio_offer(&call_id(), &call_info)?;
        let params = build_start_audio_parameters(
            &host,
            audio.local_port(),
            &phone,
            50000,
            offer,
            CLIENT_FEATURES,
            session,
        );
        client.start_media_stream(with_timeout(params)).await?;

        let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
        let offer = h264::offer(&call_id(), &call_info, ssrc)?;
        let params = build_start_video_parameters(
            &host,
            video.local_port(),
            &phone,
            50001,
            offer,
            CLIENT_FEATURES,
            1,
            session,
        );
        client.start_media_stream(with_timeout(params)).await?;
        Ok(Screen {
            client,
            _audio: audio,
            video,
            ssrc,
        })
    }
}

struct Feedback {
    ssrc: u32,
    phone: Option<(u16, u32)>,
    asked: Option<Instant>,
    requests: u8,
}

impl Feedback {
    const NAME: &str = "controlmyiphone";

    fn new(ssrc: u32) -> Self {
        Feedback {
            ssrc,
            phone: None,
            asked: None,
            requests: 0,
        }
    }

    async fn still_watching(&self, video: &UdpSocketHandle) {
        if let Some((port, _)) = self.phone {
            let _ = video.send_to(port, build_liveness(self.ssrc, Self::NAME, &[])).await;
        }
    }

    async fn ask_for_key_frame(&mut self, video: &UdpSocketHandle) {
        let Some((port, media)) = self.phone else { return };
        if self.asked.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.asked = Some(Instant::now());
        self.requests = self.requests.wrapping_add(1);
        let request = build_keyframe_request(self.ssrc, Self::NAME, media, &[], self.requests);
        let _ = video.send_to(port, request).await;
    }
}

#[derive(Default)]
struct Controls {
    touch: Option<UniversalHidServiceClient<Box<dyn ReadWrite>>>,
    buttons: Option<IndigoHidClient<Box<dyn ReadWrite>>>,
}

impl Controls {
    async fn send(&mut self, input: Input, adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<()> {
        match input {
            Input::Touch { x, y } | Input::Lift { x, y } => {
                let state = match input {
                    Input::Touch { .. } => TOUCHSCREEN_STATE_CONTACT,
                    _ => TOUCHSCREEN_STATE_RELEASE,
                };
                if self.touch.is_none() {
                    self.touch = Some(UniversalHidServiceClient::connect_rsd(adapter, handshake).await?);
                }
                let touch = self.touch.as_mut().expect("just connected");
                if let Err(e) = touch.send_touchscreen(state, x, y, None).await {
                    // a dropped connection would fail every touch after it, reconnect next time
                    self.touch = None;
                    return Err(e.into());
                }
            }
            Input::Button(button) => {
                if self.buttons.is_none() {
                    self.buttons = Some(IndigoHidClient::connect_rsd(adapter, handshake).await?);
                }
                let buttons = self.buttons.as_mut().expect("just connected");
                let (page, usage, hold, presses) = button.usage();
                let pressed = async {
                    for press in 0..presses {
                        if press > 0 {
                            tokio::time::sleep(Duration::from_millis(hold)).await;
                        }
                        buttons.send_button(page, usage, ButtonState::Down).await?;
                        tokio::time::sleep(Duration::from_millis(hold)).await;
                        buttons.send_button(page, usage, ButtonState::Up).await?;
                    }
                    Ok::<(), IdeviceError>(())
                };
                if let Err(e) = pressed.await {
                    self.buttons = None;
                    return Err(e.into());
                }
            }
            Input::Key { usage, down } => {
                if self.buttons.is_none() {
                    self.buttons = Some(IndigoHidClient::connect_rsd(adapter, handshake).await?);
                }
                let keys = self.buttons.as_mut().expect("just connected");
                let state = if down { ButtonState::Down } else { ButtonState::Up };
                if let Err(e) = keys.send_keyboard(usage as u64, state).await {
                    self.buttons = None;
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }
}

// a fresh depacketizer after a loss no longer has the sps and pps to put in front of the next key frame
#[derive(Default)]
struct Setup(Vec<u8>);

impl Setup {
    fn attach(&mut self, frame: Vec<u8>) -> Vec<u8> {
        let sets: Vec<u8> = nal_units(&frame)
            .filter(|nal| matches!(nal[0] & 0x1F, 7 | 8))
            .flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat())
            .collect();
        if !sets.is_empty() {
            self.0 = sets;
            return frame;
        }
        [self.0.as_slice(), &frame].concat()
    }
}

fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).chain([data.len()]).collect();
    starts.into_iter().zip(ends).filter_map(move |(start, end)| {
        // a 4 byte start code leaves its first zero on the end of the nal before
        let end = data[..end]
            .iter()
            .rposition(|&b| b != 0)
            .map_or(start, |p| p + 1)
            .max(start);
        (end > start).then(|| &data[start..end])
    })
}

async fn devices() -> Result<Vec<UsbmuxdDevice>> {
    let mut usbmuxd = UsbmuxdConnection::default().await?;
    Ok(usbmuxd.get_devices().await?)
}

async fn provider(udid: &str) -> Result<impl IdeviceProvider> {
    let mut matching: Vec<UsbmuxdDevice> = devices().await?.into_iter().filter(|d| d.udid == udid).collect();
    matching.sort_by_key(|d| !matches!(d.connection_type, Connection::Usb));
    let device = matching
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound(udid.to_string()))?;
    Ok(device.to_provider(UsbmuxdAddr::default(), LABEL))
}

async fn lockdown_value_in(provider: &dyn IdeviceProvider, key: &str, domain: Option<&str>) -> Result<plist::Value> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    match lockdown.get_value(Some(key), domain).await {
        Ok(v) => Ok(v),
        Err(_) => {
            lockdown.start_session(&provider.get_pairing_file().await?).await?;
            Ok(lockdown.get_value(Some(key), domain).await?)
        }
    }
}

async fn lockdown_string(provider: &dyn IdeviceProvider, key: &str) -> Option<String> {
    lockdown_value_in(provider, key, None)
        .await
        .ok()?
        .as_string()
        .map(str::to_string)
}

async fn developer_mode(provider: &dyn IdeviceProvider) -> Option<bool> {
    lockdown_value_in(provider, "DeveloperModeStatus", Some("com.apple.security.mac.amfi"))
        .await
        .ok()?
        .as_boolean()
}

fn has_developer_tools(mounted: &[plist::Value]) -> bool {
    // lookup_image("Personalized") answers NotFound even while the image is mounted, so read the mount list
    mounted.iter().any(|image| {
        image
            .as_dictionary()
            .and_then(|d| d.get("DiskImageType"))
            .and_then(plist::Value::as_string)
            == Some("Personalized")
    })
}

async fn chip_id(provider: &dyn IdeviceProvider) -> Result<u64> {
    lockdown_value_in(provider, "UniqueChipID", None)
        .await?
        .as_unsigned_integer()
        .ok_or_else(|| Error::Other("The iPhone didn't report its chip ID.".into()))
}

// over usb the upload works through lockdown but the tunnel's userspace tcp times out on it, over wi-fi it's the
// other way round, so false here means try `load_developer_tools_tunneled`
async fn load_developer_tools(provider: &dyn IdeviceProvider) -> Result<bool> {
    let mut mounter = ImageMounter::connect(provider).await?;
    if has_developer_tools(&mounter.copy_devices().await?) {
        return Ok(true);
    }
    let [image, trust_cache, manifest] = developer_disk_image().await?;
    let chip_id = chip_id(provider).await?;
    match mounter
        .mount_personalized(provider, image, trust_cache, &manifest, None, chip_id)
        .await
    {
        Ok(()) => {
            eprintln!("Loaded the developer tools on the iPhone.");
            Ok(true)
        }
        Err(IdeviceError::DeviceLocked) => Err(IdeviceError::DeviceLocked.into()),
        Err(_) => Ok(false),
    }
}

async fn load_developer_tools_tunneled(
    provider: &dyn IdeviceProvider,
    adapter: &mut impl RsdProvider,
    handshake: &mut RsdHandshake,
) -> Result<()> {
    let mut mounter = ImageMounter::connect_rsd(adapter, handshake).await?;
    if has_developer_tools(&mounter.copy_devices().await?) {
        return Ok(());
    }
    let [image, trust_cache, manifest] = developer_disk_image().await?;
    let chip_id = chip_id(provider).await?;
    mounter
        .mount_personalized_rsd(adapter, handshake, image, trust_cache, &manifest, None, chip_id)
        .await?;
    eprintln!("Loaded the developer tools on the iPhone.");
    Ok(())
}

async fn developer_disk_image() -> Result<[Vec<u8>; 3]> {
    let dir = app_dir().join("DeveloperDiskImage");
    tokio::fs::create_dir_all(&dir).await?;
    let mut files: Vec<Vec<u8>> = Vec::new();
    for name in DDI_FILES {
        let path = dir.join(name);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ => {
                let response = idevice::http::get(format!("{DDI_URL}/{name}")).await?;
                if response.status != 200 {
                    return Err(Error::Other(format!(
                        "Couldn't download Apple's developer files ({name}: HTTP {}). Check the internet connection.",
                        response.status
                    )));
                }
                tokio::fs::write(&path, &response.body).await?;
                response.body
            }
        };
        files.push(bytes);
    }
    Ok(files.try_into().expect("three files"))
}

pub fn app_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("controlmyiphone")
}
