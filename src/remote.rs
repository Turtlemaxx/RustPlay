//! Remote control: the PC poses as a Bluetooth LE mouse and keyboard (HID over
//! GATT) so an iPad paired with it can be driven from the mirrored window.
//! Experimental: it relies on Windows publishing the HID service in the
//! peripheral role, which the API allows but Apple doesn't document.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicProperties, GattLocalCharacteristic, GattLocalCharacteristicParameters,
    GattLocalDescriptorParameters, GattProtectionLevel, GattReadRequestedEventArgs, GattServiceProvider,
    GattServiceProviderAdvertisementStatus, GattServiceProviderAdvertisingParameters, GattWriteOption,
    GattWriteRequestedEventArgs,
};
use windows::Devices::Bluetooth::{
    BluetoothAdapter, BluetoothConnectionStatus, BluetoothDevice, BluetoothError, BluetoothLEDevice,
};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataWriter, IBuffer};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::core::{GUID, HSTRING, Ref, Result};

use crate::airplay::hub::Hub;

const KEYBOARD_REPORT: u8 = 1;
const MOUSE_REPORT: u8 = 2;

/// Mouse reports go out on a fixed grid, exactly three per Bluetooth connection
/// event (15 ms on iPad), so every event carries the same share of movement.
/// Sending more often would queue reports up in the radio.
const MOUSE_PACE: Duration = Duration::from_millis(5);

/// One keyboard (report 1: modifiers, reserved, six keys; LED output) and one
/// mouse (report 2: five buttons, 16-bit X/Y, wheel, horizontal pan).
#[rustfmt::skip]
const REPORT_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, KEYBOARD_REPORT,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
    0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x91, 0x01,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00,
    0xC0,
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, MOUSE_REPORT, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0xC0, 0xC0,
];

enum Command {
    Enable(bool),
    Mouse {
        dx: i32,
        dy: i32,
        buttons: u8,
        wheel: i32,
        pan: i32,
    },
    Keys {
        modifiers: u8,
        keys: [u8; 6],
    },
    /// Switches off before the app quits, then reports back.
    Shutdown(Sender<()>),
}

pub struct Remote {
    tx: Mutex<Sender<Command>>,
    state: Arc<Mutex<Value>>,
}

impl Remote {
    pub fn start(hub: Arc<Hub>) -> Arc<Remote> {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(json!({ "type": "remote", "state": "off" })));
        let shared = state.clone();
        std::thread::Builder::new()
            .name("bluetooth-hid".into())
            .spawn(move || run(rx, hub, shared))
            .expect("hid thread");
        Arc::new(Remote { tx: Mutex::new(tx), state })
    }

    /// Disconnects the iPad before the app quits; otherwise it stays linked to the PC.
    pub fn shutdown(&self) {
        let (done, finished) = mpsc::channel();
        if self.tx.lock().unwrap().send(Command::Shutdown(done)).is_ok() {
            let _ = finished.recv_timeout(Duration::from_secs(8));
        }
    }

    /// Commands from the window; mouse and key updates arrive without a reply.
    pub fn command(&self, request: &Value) -> Value {
        let tx = self.tx.lock().unwrap();
        let int = |key: &str| request[key].as_f64().unwrap_or(0.0) as i32;
        match request["type"].as_str().unwrap_or_default() {
            "remote-status" => return self.state.lock().unwrap().clone(),
            "remote-enable" => {
                let _ = tx.send(Command::Enable(request["on"].as_bool().unwrap_or(false)));
            }
            "remote-mouse" => {
                let buttons = request["buttons"].as_u64().unwrap_or(0) as u8;
                let _ = tx.send(Command::Mouse {
                    dx: int("dx"),
                    dy: int("dy"),
                    buttons,
                    wheel: int("wheel"),
                    pan: int("pan"),
                });
            }
            "remote-keys" => {
                let mut keys = [0u8; 6];
                for (slot, key) in keys.iter_mut().zip(request["keys"].as_array().into_iter().flatten()) {
                    *slot = key.as_u64().unwrap_or(0) as u8;
                }
                let _ = tx.send(Command::Keys { modifiers: request["modifiers"].as_u64().unwrap_or(0) as u8, keys });
            }
            _ => return json!({ "ok": false, "error": "unknown command" }),
        }
        json!({ "ok": true })
    }
}

fn run(rx: Receiver<Command>, hub: Arc<Hub>, state: Arc<Mutex<Value>>) {
    let publish = |value: Value| {
        *state.lock().unwrap() = value.clone();
        hub.notify(value);
    };
    // Every iPad that connected since launch; switching off disconnects them.
    let centrals = Arc::new(Mutex::new(Vec::new()));
    let mut device: Option<HidDevice> = None;
    let mut held: Option<Command> = None;
    let (mut next_report, mut last_buttons) = (Instant::now(), 0);
    loop {
        let Some(command) = held.take().or_else(|| rx.recv().ok()) else { return };
        match command {
            Command::Enable(true) if device.is_none() => {
                match HidDevice::publish(hub.clone(), state.clone(), centrals.clone()) {
                    Ok(published) => {
                        log::info!("Bluetooth mouse and keyboard advertising");
                        device = Some(published);
                        publish(advertising_state());
                    }
                    Err(err) => {
                        log::warn!("Bluetooth mouse and keyboard unavailable: {err}");
                        publish(json!({ "type": "remote", "state": "error", "detail": err.message().to_string() }));
                    }
                }
            }
            Command::Enable(false) => {
                drop(device.take());
                let mut off = json!({ "type": "remote", "state": "off" });
                if !disconnect(&centrals.lock().unwrap().clone()) {
                    off["detail"] = "Your iPad is still connected because other Bluetooth devices are in use. \
                                     Disconnect it in the iPad's Bluetooth settings."
                        .into();
                }
                publish(off);
            }
            Command::Shutdown(done) => {
                drop(device.take());
                disconnect(&centrals.lock().unwrap().clone());
                let _ = done.send(());
                return;
            }
            Command::Enable(true) => {}
            Command::Mouse { mut dx, mut dy, buttons, mut wheel, mut pan } => {
                // Movement folds in until the next slot on the grid. After a pause,
                // or for a press or release, the grid restarts from now.
                let now = Instant::now();
                if buttons != last_buttons || now > next_report + MOUSE_PACE {
                    next_report = now;
                }
                loop {
                    let wait = next_report.saturating_duration_since(Instant::now());
                    let next = if wait.is_zero() { rx.try_recv().ok() } else { rx.recv_timeout(wait).ok() };
                    match next {
                        Some(Command::Mouse { dx: x, dy: y, buttons: b, wheel: w, pan: p }) if b == buttons => {
                            dx += x;
                            dy += y;
                            wheel += w;
                            pan += p;
                        }
                        Some(other) => {
                            held = Some(other);
                            break;
                        }
                        None => break,
                    }
                }
                if let Some(device) = &device {
                    device.mouse(dx, dy, buttons, wheel, pan);
                }
                next_report += MOUSE_PACE;
                last_buttons = buttons;
            }
            Command::Keys { modifiers, keys } => {
                if let Some(device) = &device {
                    device.keys(modifiers, keys);
                }
            }
        }
    }
}

fn advertising_state() -> Value {
    let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this PC".into());
    json!({ "type": "remote", "state": "advertising", "name": name })
}

/// The published GATT services; dropping it stops advertising.
struct HidDevice {
    hid: GattServiceProvider,
    battery: GattServiceProvider,
    keyboard: GattLocalCharacteristic,
    mouse: GattLocalCharacteristic,
}

impl Drop for HidDevice {
    fn drop(&mut self) {
        let _ = self.hid.StopAdvertising();
        let _ = self.battery.StopAdvertising();
        // SAFETY: balances the timeBeginPeriod in publish.
        unsafe { timeEndPeriod(1) };
    }
}

impl HidDevice {
    fn publish(hub: Arc<Hub>, state: Arc<Mutex<Value>>, centrals: Arc<Mutex<Vec<HSTRING>>>) -> Result<Self> {
        let adapter = BluetoothAdapter::GetDefaultAsync()?.join()?;
        if !adapter.IsPeripheralRoleSupported()? {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(0x8000_4001u32 as i32),
                "This PC's Bluetooth can't act as a mouse or keyboard.",
            ));
        }

        let battery = provider(0x180F)?;
        characteristic(&battery, 0x2A19, GattCharacteristicProperties::Read, Some(&[100]), false)?;

        let hid = provider(0x1812)?;
        characteristic(&hid, 0x2A4A, GattCharacteristicProperties::Read, Some(&[0x11, 0x01, 0x00, 0x02]), true)?;
        characteristic(&hid, 0x2A4B, GattCharacteristicProperties::Read, Some(REPORT_MAP), true)?;
        let control = characteristic(&hid, 0x2A4C, GattCharacteristicProperties::WriteWithoutResponse, None, true)?;
        accept_writes(&control)?;

        let read_notify = flags(&[GattCharacteristicProperties::Read, GattCharacteristicProperties::Notify]);
        let keyboard = characteristic(&hid, 0x2A4D, read_notify, None, true)?;
        report_reference(&keyboard, KEYBOARD_REPORT, 1)?;
        answer_reads(&keyboard, vec![0; 8])?;

        let output_flags = flags(&[
            GattCharacteristicProperties::Read,
            GattCharacteristicProperties::Write,
            GattCharacteristicProperties::WriteWithoutResponse,
        ]);
        let leds = characteristic(&hid, 0x2A4D, output_flags, None, true)?;
        report_reference(&leds, KEYBOARD_REPORT, 2)?;
        answer_reads(&leds, vec![0])?;
        accept_writes(&leds)?;

        let mouse = characteristic(&hid, 0x2A4D, read_notify, None, true)?;
        report_reference(&mouse, MOUSE_REPORT, 1)?;
        answer_reads(&mouse, vec![0; 7])?;

        // The device counts as connected once it subscribes to mouse reports.
        mouse.SubscribedClientsChanged(&TypedEventHandler::new(move |sender: Ref<GattLocalCharacteristic>, _| {
            let subscribed = sender.ok()?.SubscribedClients()?;
            let clients = subscribed.Size()?;
            for client in subscribed {
                let id = client.Session()?.DeviceId()?.Id()?;
                let mut known = centrals.lock().unwrap();
                if !known.contains(&id) {
                    log::info!("Bluetooth central subscribed: {id}");
                    known.push(id);
                }
            }
            let value = if clients > 0 {
                log::info!("Bluetooth mouse and keyboard connected");
                json!({ "type": "remote", "state": "connected" })
            } else {
                advertising_state()
            };
            *state.lock().unwrap() = value.clone();
            hub.notify(value);
            Ok(())
        }))?;

        let advertising = GattServiceProviderAdvertisingParameters::new()?;
        advertising.SetIsConnectable(true)?;
        advertising.SetIsDiscoverable(false)?;
        battery.StartAdvertisingWithParameters(&advertising)?;
        let discoverable = GattServiceProviderAdvertisingParameters::new()?;
        discoverable.SetIsConnectable(true)?;
        discoverable.SetIsDiscoverable(true)?;
        hid.StartAdvertisingWithParameters(&discoverable)?;
        let device = HidDevice { hid, battery, keyboard, mouse };
        // MOUSE_PACE needs millisecond waits; at Windows' default 15.6 ms timer
        // they'd round up and bunch reports together again. Undone on drop.
        // SAFETY: plain Win32 call, balanced in Drop.
        unsafe { timeBeginPeriod(1) };

        // Windows starts advertising asynchronously and may refuse (e.g. Bluetooth off).
        // It briefly reports Aborted before Started, so only the outcome after a grace period counts.
        for _ in 0..60 {
            match device.hid.AdvertisementStatus()? {
                GattServiceProviderAdvertisementStatus::Started
                | GattServiceProviderAdvertisementStatus::StartedWithoutAllAdvertisementData => return Ok(device),
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        Err(windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005u32 as i32),
            "Windows didn't start Bluetooth advertising. Check that Bluetooth is on.",
        ))
    }

    fn mouse(&self, dx: i32, dy: i32, buttons: u8, wheel: i32, pan: i32) {
        log::trace!("mouse report {dx} {dy} buttons {buttons:#04x} wheel {wheel} pan {pan}");
        let (dx, dy) = (dx.clamp(-32767, 32767) as i16, dy.clamp(-32767, 32767) as i16);
        let mut report = vec![buttons & 0x1F];
        report.extend_from_slice(&dx.to_le_bytes());
        report.extend_from_slice(&dy.to_le_bytes());
        report.push(wheel.clamp(-127, 127) as i8 as u8);
        report.push(pan.clamp(-127, 127) as i8 as u8);
        notify(&self.mouse, &report);
    }

    fn keys(&self, modifiers: u8, keys: [u8; 6]) {
        let mut report = vec![modifiers, 0];
        report.extend_from_slice(&keys);
        notify(&self.keyboard, &report);
    }
}

fn notify(characteristic: &GattLocalCharacteristic, report: &[u8]) {
    let started = Instant::now();
    let sent = buffer(report).and_then(|value| characteristic.NotifyValueAsync(&value)?.join());
    let took = started.elapsed();
    match sent {
        Err(err) => log::debug!("HID report not delivered: {err}"),
        Ok(_) if took > Duration::from_millis(25) => log::debug!("HID report took {} ms", took.as_millis()),
        Ok(results) => log::trace!(
            "HID report sent in {} µs to {:?}",
            took.as_micros(),
            results.into_iter().map(|result| result.Status().map(|status| status.0)).collect::<Vec<_>>()
        ),
    }
}

/// Drops the Bluetooth link to these iPads so they show as not connected, and
/// reconnect by themselves once switched back on. Windows has no API for a
/// peripheral to end a link (the driver's disconnect request only knows classic
/// Bluetooth), so this switches Bluetooth off and on, unless that would also cut
/// off other devices. Returns whether no link is left.
fn disconnect(centrals: &[HSTRING]) -> bool {
    let linked = linked(centrals);
    log::info!("Disconnecting: {} known, linked {linked:012x?}", centrals.len());
    if linked.is_empty() {
        return true;
    }
    if others_connected(&linked) {
        log::info!("iPad still connected; other Bluetooth devices are in use, so Bluetooth stays on");
        return false;
    }
    restart_radio();
    let gone = wait_unlinked(centrals, Duration::from_secs(4));
    log::info!("Restarted Bluetooth to disconnect the iPad: {}", if gone { "done" } else { "still connected" });
    gone
}

/// Addresses of these devices that still have a link to this PC.
fn linked(centrals: &[HSTRING]) -> Vec<u64> {
    centrals
        .iter()
        .filter_map(|id| BluetoothLEDevice::FromIdAsync(id).ok()?.join().ok())
        .filter(|device| device.ConnectionStatus().ok() == Some(BluetoothConnectionStatus::Connected))
        .filter_map(|device| device.BluetoothAddress().ok())
        .collect()
}

fn wait_unlinked(centrals: &[HSTRING], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if linked(centrals).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a Bluetooth device other than these is connected (headphones, a controller…).
fn others_connected(ours: &[u64]) -> bool {
    let check = || -> Result<bool> {
        let classic = BluetoothDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected)?;
        for info in DeviceInformation::FindAllAsyncAqsFilter(&classic)?.join()? {
            let address = BluetoothDevice::FromIdAsync(&info.Id()?)?.join()?.BluetoothAddress()?;
            if !ours.contains(&address) {
                return Ok(true);
            }
        }
        let le = BluetoothLEDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected)?;
        for info in DeviceInformation::FindAllAsyncAqsFilter(&le)?.join()? {
            let address = BluetoothLEDevice::FromIdAsync(&info.Id()?)?.join()?.BluetoothAddress()?;
            if !ours.contains(&address) {
                return Ok(true);
            }
        }
        Ok(false)
    };
    // When unsure, assume something else is in use and leave Bluetooth alone.
    check().unwrap_or(true)
}

/// Switches Bluetooth off and straight back on, which ends every link.
fn restart_radio() {
    let find = || -> Result<Option<Radio>> {
        if Radio::RequestAccessAsync()?.join()? != RadioAccessStatus::Allowed {
            return Ok(None);
        }
        for radio in Radio::GetRadiosAsync()?.join()? {
            if radio.Kind()? == RadioKind::Bluetooth && radio.State()? == RadioState::On {
                return Ok(Some(radio));
            }
        }
        Ok(None)
    };
    let Ok(Some(radio)) = find() else {
        log::warn!("Couldn't restart Bluetooth to disconnect the iPad");
        return;
    };
    let _ = radio.SetStateAsync(RadioState::Off).and_then(|change| change.join());
    std::thread::sleep(Duration::from_millis(300));
    // Bluetooth has to come back on, so keep trying for a few seconds.
    for _ in 0..10 {
        let _ = radio.SetStateAsync(RadioState::On).and_then(|change| change.join());
        if radio.State().ok() == Some(RadioState::On) {
            return;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    log::warn!("Bluetooth didn't come back on after disconnecting the iPad");
}

/// A Bluetooth SIG 16-bit UUID in its full 128-bit form.
fn uuid16(short: u16) -> GUID {
    GUID::from_values(short as u32, 0x0000, 0x1000, [0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB])
}

fn buffer(bytes: &[u8]) -> Result<IBuffer> {
    let writer = DataWriter::new()?;
    writer.WriteBytes(bytes)?;
    writer.DetachBuffer()
}

fn flags(all: &[GattCharacteristicProperties]) -> GattCharacteristicProperties {
    GattCharacteristicProperties(all.iter().fold(0, |acc, f| acc | f.0))
}

fn provider(service: u16) -> Result<GattServiceProvider> {
    let result = GattServiceProvider::CreateAsync(uuid16(service))?.join()?;
    if result.Error()? != BluetoothError::Success {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005u32 as i32),
            format!("Couldn't publish Bluetooth service {service:#06x}"),
        ));
    }
    result.ServiceProvider()
}

/// HID data only flows over an encrypted (paired) link.
fn characteristic(
    provider: &GattServiceProvider,
    uuid: u16,
    properties: GattCharacteristicProperties,
    value: Option<&[u8]>,
    encrypted: bool,
) -> Result<GattLocalCharacteristic> {
    let level = if encrypted { GattProtectionLevel::EncryptionRequired } else { GattProtectionLevel::Plain };
    let parameters = GattLocalCharacteristicParameters::new()?;
    parameters.SetCharacteristicProperties(properties)?;
    parameters.SetReadProtectionLevel(level)?;
    parameters.SetWriteProtectionLevel(level)?;
    if let Some(value) = value {
        parameters.SetStaticValue(&buffer(value)?)?;
    }
    provider.Service()?.CreateCharacteristicAsync(uuid16(uuid), &parameters)?.join()?.Characteristic()
}

/// Report Reference descriptor: which report ID this characteristic carries, and
/// whether it's input (1) or output (2).
fn report_reference(characteristic: &GattLocalCharacteristic, id: u8, kind: u8) -> Result<()> {
    let parameters = GattLocalDescriptorParameters::new()?;
    parameters.SetStaticValue(&buffer(&[id, kind])?)?;
    parameters.SetReadProtectionLevel(GattProtectionLevel::EncryptionRequired)?;
    characteristic.CreateDescriptorAsync(uuid16(0x2908), &parameters)?.join()?.Descriptor()?;
    Ok(())
}

fn answer_reads(characteristic: &GattLocalCharacteristic, value: Vec<u8>) -> Result<()> {
    characteristic.ReadRequested(&TypedEventHandler::new(move |_, args: Ref<GattReadRequestedEventArgs>| {
        let args = args.ok()?;
        let deferral = args.GetDeferral()?;
        let request = args.GetRequestAsync()?.join()?;
        request.RespondWithValue(&buffer(&value)?)?;
        deferral.Complete()
    }))?;
    Ok(())
}

/// Control-point and LED writes need no action, just an acknowledgement.
fn accept_writes(characteristic: &GattLocalCharacteristic) -> Result<()> {
    characteristic.WriteRequested(&TypedEventHandler::new(|_, args: Ref<GattWriteRequestedEventArgs>| {
        let args = args.ok()?;
        let deferral = args.GetDeferral()?;
        let request = args.GetRequestAsync()?.join()?;
        if request.Option()? == GattWriteOption::WriteWithResponse {
            request.Respond()?;
        }
        deferral.Complete()
    }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks the report descriptor's short items to make sure it's well formed:
    /// every item's payload fits and collections open and close in pairs.
    #[test]
    fn report_map_is_well_formed() {
        let mut pos = 0;
        let mut depth = 0i32;
        while pos < REPORT_MAP.len() {
            let prefix = REPORT_MAP[pos];
            let size = match prefix & 0x03 {
                3 => 4,
                n => n as usize,
            };
            match prefix & 0xFC {
                0xA0 => depth += 1,
                0xC0 => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "collection closed too early at byte {pos}");
            pos += 1 + size;
        }
        assert_eq!(pos, REPORT_MAP.len(), "last item runs past the end");
        assert_eq!(depth, 0, "unclosed collection");
    }
}
