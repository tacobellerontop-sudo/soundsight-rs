//! WASAPI shared-mode loopback capture of the default render device, event driven.
//!
//! Anti-cheat note: this only uses the public Core Audio API on the *audio endpoint*
//! (what the speakers/headset receive). It never opens the game process, never reads
//! or writes another process's memory and never injects or hooks anything.
//!
//! Threading: one dedicated thread, ABOVE_NORMAL priority (optional MMCSS "Games").
//! It sleeps in `WaitForMultipleObjects` on two events:
//!   * the WASAPI buffer event (a packet is ready, ~every 10 ms while audio plays)
//!   * a control event (device changed, config reload, show/hide, quit)
//!
//! The only timeout is used while bars are fading out and no packets arrive (loopback
//! streams deliver nothing while the system is silent). When everything is faded and
//! silent, the wait is INFINITE: zero CPU.

use crate::config::Config;
use crate::dsp::analyzer::{Analyzer, BINS};
use crate::dsp::layout::{Layout, MAX_CHANNELS};
use crate::state::Shared;
use std::sync::atomic::Ordering::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{implement, w, Result, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, PROPERTYKEY, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Posted to `Shared::notify_hwnd` when bins/meters changed.
pub const WM_APP_REDRAW: u32 = WM_APP + 1;
/// Posted when the stream (re)started or stopped, so the UI can refresh format info.
pub const WM_APP_STREAM: u32 = WM_APP + 2;

/// 20 ms shared-mode buffer: room for scheduling jitter; the engine still signals every period.
const BUFFER_HNS: i64 = 200_000;
/// While bars fade with no packets, wake this often to advance the fade.
const FADE_TICK_MS: u32 = 16;

/// Handle owned by the UI thread.
pub struct CaptureThread {
    control: HANDLE,
    shared: Arc<Shared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CaptureThread {
    pub fn spawn(shared: Arc<Shared>, config: Arc<Mutex<Config>>) -> Result<CaptureThread> {
        // Auto-reset event; HANDLE is not Send, so move the raw value.
        let control = unsafe { CreateEventW(None, false, false, None)? };
        let raw = control.0 as isize;
        let sh = shared.clone();
        let join = std::thread::Builder::new()
            .name("soundsight-capture".into())
            .stack_size(256 * 1024)
            .spawn(move || capture_thread(sh, config, HANDLE(raw as _)))
            .expect("spawn capture thread");
        Ok(CaptureThread {
            control,
            shared,
            join: Some(join),
        })
    }

    /// Wake the thread so it re-checks `Shared::{wanted, reinit_requested, quit}`.
    pub fn wake(&self) {
        unsafe {
            let _ = SetEvent(self.control);
        }
    }

    pub fn request_reinit(&self) {
        self.shared.reinit_requested.store(true, SeqCst);
        self.wake();
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.shared.quit.store(true, SeqCst);
        self.wake();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        unsafe {
            let _ = CloseHandle(self.control);
        }
    }
}

// ---------------------------------------------------------------------------
// Device change notifications
// ---------------------------------------------------------------------------

/// Receives endpoint notifications on a system thread. It must not call back into
/// the audio API: it only flags a re-init and signals the capture thread.
#[implement(IMMNotificationClient)]
struct DeviceNotifier {
    shared: Arc<Shared>,
    control: isize,
    /// Endpoint ID of the device we are capturing from (UTF-16, no terminator).
    current_id: Arc<Mutex<Vec<u16>>>,
}

impl DeviceNotifier {
    fn trigger(&self) {
        self.shared.reinit_requested.store(true, SeqCst);
        unsafe {
            let _ = SetEvent(HANDLE(self.control as _));
        }
    }

    fn is_current(&self, id: &PCWSTR) -> bool {
        if id.is_null() {
            return false;
        }
        let id = unsafe { id.as_wide() };
        self.current_id
            .lock()
            .map(|c| c.as_slice() == id)
            .unwrap_or(false)
    }
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> Result<()> {
        if self.is_current(id) && state != DEVICE_STATE_ACTIVE {
            self.trigger();
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> Result<()> {
        if self.is_current(id) {
            self.trigger();
        }
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> Result<()> {
        if flow == eRender && role == eConsole {
            self.trigger();
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum SampleKind {
    F32,
    I16,
    /// 32-bit container (also used for 24-bit-in-32, which is left aligned).
    I32,
}

#[derive(Clone, Copy, Debug)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: usize,
    pub channel_mask: u32,
    kind: SampleKind,
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: StreamFormat,
    running: bool,
}

impl Stream {
    fn open(
        enumerator: &IMMDeviceEnumerator,
        audio_event: HANDLE,
        current_id: &Mutex<Vec<u16>>,
    ) -> Result<Stream> {
        unsafe {
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let id = device.GetId()?;
            if let Ok(mut cur) = current_id.lock() {
                cur.clear();
                cur.extend_from_slice(id.as_wide());
            }
            CoTaskMemFree(Some(id.0 as _));

            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let pwfx = client.GetMixFormat()?;
            let format = parse_format(pwfx);
            let init = match format {
                Some(_) => client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    BUFFER_HNS,
                    0,
                    pwfx,
                    None,
                ),
                None => Err(windows::core::Error::from_hresult(
                    AUDCLNT_E_UNSUPPORTED_FORMAT,
                )),
            };
            CoTaskMemFree(Some(pwfx as _));
            init?;
            client.SetEventHandle(audio_event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            Ok(Stream {
                client,
                capture,
                format: format.unwrap(),
                running: false,
            })
        }
    }

    fn start(&mut self) -> Result<()> {
        if !self.running {
            unsafe { self.client.Start()? };
            self.running = true;
        }
        Ok(())
    }

    fn stop(&mut self) {
        if self.running {
            unsafe {
                let _ = self.client.Stop();
                let _ = self.client.Reset();
            }
            self.running = false;
        }
    }

    /// Drain every pending packet into the analyser. Allocation free.
    #[inline]
    fn drain(&mut self, analyzer: &mut Analyzer) -> Result<()> {
        let ch = self.format.channels;
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * ch;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    analyzer.process_silence(frames as usize);
                } else {
                    match self.format.kind {
                        SampleKind::F32 => {
                            analyzer.process(std::slice::from_raw_parts(data as *const f32, n))
                        }
                        SampleKind::I16 => {
                            analyzer.process(std::slice::from_raw_parts(data as *const i16, n))
                        }
                        SampleKind::I32 => {
                            analyzer.process(std::slice::from_raw_parts(data as *const i32, n))
                        }
                    };
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read the mix format (WAVEFORMATEX or WAVEFORMATEXTENSIBLE; both are packed structs).
fn parse_format(pwfx: *const WAVEFORMATEX) -> Option<StreamFormat> {
    unsafe {
        let wfx = std::ptr::read_unaligned(pwfx);
        let tag = wfx.wFormatTag as u32;
        let bits = wfx.wBitsPerSample;
        let channels = wfx.nChannels as usize;
        let (mask, is_float) = if tag == WAVE_FORMAT_EXTENSIBLE && wfx.cbSize >= 22 {
            let ext = std::ptr::read_unaligned(pwfx as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            (ext.dwChannelMask, sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)
        } else {
            (0, tag == WAVE_FORMAT_IEEE_FLOAT)
        };
        let kind = match (is_float, bits) {
            (true, 32) => SampleKind::F32,
            (false, 16) => SampleKind::I16,
            (false, 32) => SampleKind::I32,
            _ => return None, // 24-bit packed / 64-bit float: not used by the shared-mode engine in practice
        };
        if channels == 0 {
            return None;
        }
        Some(StreamFormat {
            sample_rate: wfx.nSamplesPerSec,
            channels,
            channel_mask: mask,
            kind,
        })
    }
}

// ---------------------------------------------------------------------------
// Thread body
// ---------------------------------------------------------------------------

enum Exit {
    Quit,
    Reinit,
    NotWanted,
    DeviceLost,
}

fn capture_thread(shared: Arc<Shared>, config: Arc<Mutex<Config>>, control: HANDLE) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Elevated, but well below real-time: we must never compete with the game's render thread.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }

    let mut mmcss: Option<HANDLE> = None;
    if config.lock().map(|c| c.general.mmcss).unwrap_or(false) {
        let mut task = 0u32;
        mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Games"), &mut task).ok() };
    }

    if let Err(e) = run(&shared, &config, control) {
        eprintln!("capture thread failed to start: {e}");
    }

    if let Some(h) = mmcss {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
    shared.capturing.store(false, SeqCst);
    unsafe { CoUninitialize() };
}

fn run(shared: &Arc<Shared>, config: &Arc<Mutex<Config>>, control: HANDLE) -> Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let current_id = Arc::new(Mutex::new(Vec::with_capacity(128)));
    let notifier: IMMNotificationClient = DeviceNotifier {
        shared: shared.clone(),
        control: control.0 as isize,
        current_id: current_id.clone(),
    }
    .into();
    unsafe { enumerator.RegisterEndpointNotificationCallback(&notifier)? };

    let audio_event = unsafe { CreateEventW(None, false, false, None)? };

    loop {
        if shared.quit.load(SeqCst) {
            break;
        }
        shared.reinit_requested.store(false, SeqCst);

        if !shared.wanted.load(SeqCst) {
            set_stopped(shared);
            wait_one(control, u32::MAX);
            continue;
        }

        let cfg = config.lock().map(|c| c.clone()).unwrap_or_default();
        let mut stream = match Stream::open(&enumerator, audio_event, &current_id) {
            Ok(s) => s,
            Err(e) => {
                // No render device / device busy. A device notification will wake us;
                // the timeout is only a safety net.
                eprintln!("capture: cannot open default render device: {e}");
                set_stopped(shared);
                wait_one(control, 5000);
                continue;
            }
        };

        let f = stream.format;
        let layout = Layout::from_mask(f.channels, f.channel_mask, &cfg.speakers);
        shared.channels.store(f.channels as u32, SeqCst);
        shared.channel_mask.store(layout.mask, SeqCst);
        shared.sample_rate.store(f.sample_rate, SeqCst);
        let mut analyzer = Analyzer::new(layout, f.sample_rate, &cfg);

        let exit = match stream.start() {
            Ok(()) => {
                shared.capturing.store(true, SeqCst);
                notify(shared, WM_APP_STREAM);
                pump(shared, &mut stream, &mut analyzer, audio_event, control)
            }
            Err(_) => Exit::DeviceLost,
        };
        drop(stream);

        match exit {
            Exit::Quit => break,
            Exit::NotWanted => {}
            // Device notifications tend to arrive in bursts; let them settle.
            Exit::Reinit | Exit::DeviceLost => {
                set_stopped(shared);
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }

    set_stopped(shared);
    unsafe {
        let _ = enumerator.UnregisterEndpointNotificationCallback(&notifier);
        let _ = CloseHandle(audio_event);
    }
    Ok(())
}

/// The steady-state loop. No allocation, no polling.
fn pump(
    shared: &Shared,
    stream: &mut Stream,
    analyzer: &mut Analyzer,
    audio_event: HANDLE,
    control: HANDLE,
) -> Exit {
    let handles = [audio_event, control];
    let mut qbins = [0u8; BINS];
    let mut qmeters = [0u8; MAX_CHANNELS];
    let mut last = Instant::now();

    loop {
        let timeout = if analyzer.is_idle() {
            u32::MAX
        } else {
            FADE_TICK_MS
        };
        let r = unsafe { WaitForMultipleObjects(&handles, false, timeout) };

        if r == WAIT_OBJECT_0 {
            if stream.drain(analyzer).is_err() {
                // Typically AUDCLNT_E_DEVICE_INVALIDATED (unplugged / format changed).
                return Exit::DeviceLost;
            }
            last = Instant::now();
        } else if r.0 == WAIT_OBJECT_0.0 + 1 {
            if shared.quit.load(SeqCst) {
                return Exit::Quit;
            }
            if shared.reinit_requested.load(SeqCst) {
                return Exit::Reinit;
            }
            if !shared.wanted.load(SeqCst) {
                return Exit::NotWanted;
            }
            continue;
        } else if r == WAIT_TIMEOUT {
            // No packets (system silent): keep fading the bars by wall-clock time.
            let now = Instant::now();
            analyzer.tick(now.duration_since(last).as_secs_f32());
            last = now;
        } else {
            return Exit::DeviceLost;
        }

        shared
            .stereo_content
            .store(analyzer.stereo_content(), Relaxed);
        analyzer.quantized_bins(&mut qbins);
        analyzer.quantized_meters(&mut qmeters);
        if shared.publish(&qbins, &qmeters) && !shared.redraw_pending.swap(true, AcqRel) {
            notify(shared, WM_APP_REDRAW);
        }
    }
}

fn set_stopped(shared: &Shared) {
    if shared.capturing.swap(false, SeqCst) {
        notify(shared, WM_APP_STREAM);
    }
    shared.clear();
    if !shared.redraw_pending.swap(true, AcqRel) {
        notify(shared, WM_APP_REDRAW);
    }
}

fn notify(shared: &Shared, msg: u32) {
    let hwnd = shared.notify_hwnd.load(Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn wait_one(h: HANDLE, ms: u32) {
    unsafe {
        WaitForSingleObject(h, ms);
    }
}
