//! Windows audio sessions on all active output devices: measure each app's real output, set per-app volume, restore.
//! All COM objects live on the engine thread (MTA).
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{GUID, IUnknown, Interface, PCWSTR, Ref, Result, implement};

/// Session peak meters can lie (Discord's mirrors the whole device mix), so each app's level comes from
/// process loopback: a capture of only that process tree's output (Windows 10 2004+).
pub struct ProcessMeter {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activated(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}
impl IAgileObject_Impl for Activated_Impl {}

#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const u8,
}

impl ProcessMeter {
    pub fn new(pid: u32) -> Result<Self> {
        unsafe {
            let params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            let pv = PropVariantBlob {
                vt: 65, // VT_BLOB
                reserved: [0; 3],
                size: size_of_val(&params) as u32,
                data: &params as *const _ as *const u8,
            };
            let (tx, rx) = mpsc::sync_channel(1);
            let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
            let op = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&pv as *const _ as *const _),
                &handler,
            )?;
            rx.recv_timeout(Duration::from_secs(2)).map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?;
            let (mut hr, mut unk) = (windows::core::HRESULT(0), None::<IUnknown>);
            op.GetActivateResult(&mut hr, &mut unk)?;
            hr.ok()?;
            let client: IAudioClient = unk.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER))?.cast()?;
            // Process loopback has no mix format; ask for s16 stereo at the usual mix rate (48k) so Windows needn't resample.
            let fmt = WAVEFORMATEX { wFormatTag: 1, nChannels: 2, nSamplesPerSec: 48000, nAvgBytesPerSec: 48000 * 4, nBlockAlign: 4, wBitsPerSample: 16, cbSize: 0 };
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &fmt, None)?;
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            client.SetEventHandle(event)?; // required in this mode even though we poll
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self { client, capture, event })
        }
    }

    /// Peak (0..1) of everything captured since the last call.
    pub fn peak(&self) -> Result<f32> {
        let mut peak = 0i32;
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if frames > 0 && flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 {
                    let samples = std::slice::from_raw_parts(data as *const i16, frames as usize * 2);
                    peak = samples.iter().fold(peak, |m, &s| m.max((s as i32).abs()));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok((peak as f32 / 32768.0).min(1.0))
    }
}

impl Drop for ProcessMeter {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// Event context on our volume writes, so session-event listeners can tell AudioDuck from other apps.
const CTX: GUID = GUID::from_u128(0x41d1_0d0c_7a3e_4b5f_9c2d_a0d1_0c4e_5f60);

struct Session {
    name: String,
    sid: String, // GetSessionIdentifier: app + device, survives the session being recreated
    root: u32,
    vol: ISimpleAudioVolume, // the user's own volume (what mixers show and set): read, never ducked
    // Ducking lowers the per-channel volumes instead, which multiply with the user's and no mixer touches, so the
    // user can change an app's volume mid-duck without AudioDuck overwriting it or putting an old value back.
    chan: IChannelAudioVolume,
    meter: IAudioMeterInformation,
    last_set: Option<f32>, // gain we last wrote to every channel; None = channels untouched
}

fn set_channels(chan: &IChannelAudioVolume, g: f32) -> bool {
    unsafe { chan.GetChannelCount().and_then(|n| chan.SetAllVolumes(&vec![g; n as usize], &CTX)) }.is_ok()
}

pub struct Mixer {
    sessions: HashMap<String, Session>,
    // root pid -> meter; None = loopback failed, use the session meter; absent = app not measured (reads as 0)
    loopbacks: HashMap<u32, Option<ProcessMeter>>,
    // root pid -> app name of rule sources found as running processes, measured even before they open an audio
    // session: a session only shows up on the next refresh, which made the first second of a new sound go unnoticed.
    watched: HashMap<u32, String>,
    state_path: PathBuf,
    // Windows remembers per-app channel volumes, so sessions that vanish while ducked (or all of them, if we died) get
    // full channels back when they reappear. Keyed by session identifier.
    pending_restore: HashSet<String>,
    saved: HashSet<String>,
    // Builds before channel ducking lowered the user's volume itself and left sid -> their volume in LEGACY_STATE for
    // sessions that vanished ducked; each gets it back once, when it reappears.
    legacy: HashMap<String, f32>,
}

const LEGACY_STATE: &str = "ducked_state.json";

/// pid -> (lowercase exe name, parent pid)
pub fn processes() -> HashMap<u32, (String, u32)> {
    let mut out = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.insert(e.th32ProcessID, (String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase(), e.th32ParentProcessID));
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Topmost ancestor with the same exe name, so the loopback covers the whole app (renderers, utilities).
fn root_pid(procs: &HashMap<u32, (String, u32)>, pid: u32, name: &str) -> u32 {
    let mut cur = pid;
    for _ in 0..32 {
        match procs.get(&cur).and_then(|(_, ppid)| procs.get(ppid).map(|(n, _)| (n, *ppid))) {
            Some((n, ppid)) if n == name && ppid != cur => cur = ppid,
            _ => break,
        }
    }
    cur
}

impl Mixer {
    pub fn new(state_path: PathBuf) -> Self {
        let pending: HashSet<String> = std::fs::read_to_string(&state_path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let legacy = std::fs::read_to_string(state_path.with_file_name(LEGACY_STATE)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { sessions: HashMap::new(), loopbacks: HashMap::new(), watched: HashMap::new(), state_path, saved: pending.clone(), pending_restore: pending, legacy }
    }

    /// `measure(app)`: whether that app needs an accurate level (each loopback costs a capture pipeline).
    /// `watch(app)`: a rule source, measured as soon as its process runs, with or without an audio session.
    pub fn refresh(&mut self, measure: impl Fn(&str) -> bool, watch: impl Fn(&str) -> bool) {
        let procs = processes();
        let me = std::process::id();
        let mut found = HashMap::new();
        let mut legacy_hit = false;
        unsafe {
            let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return };
            let Ok(devices) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return };
            for i in 0..devices.GetCount().unwrap_or(0) {
                let Ok(sessions) = devices.Item(i).and_then(|d| d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)).and_then(|m| m.GetSessionEnumerator()) else { continue };
                for j in 0..sessions.GetCount().unwrap_or(0) {
                    let Ok(ctl) = sessions.GetSession(j).and_then(|c| c.cast::<IAudioSessionControl2>()) else { continue };
                    let pid = ctl.GetProcessId().unwrap_or(0);
                    if pid == 0 || pid == me || ctl.GetState().is_ok_and(|s| s == AudioSessionStateExpired) {
                        continue; // pid 0 = system sounds
                    }
                    let Ok(id) = ctl.GetSessionInstanceIdentifier() else { continue };
                    let key = id.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(id.0 as _));
                    if let Some(s) = self.sessions.remove(&key) {
                        found.insert(key, s);
                        continue;
                    }
                    let Some((name, _)) = procs.get(&pid) else { continue };
                    let (Ok(vol), Ok(chan), Ok(meter)) = (ctl.cast::<ISimpleAudioVolume>(), ctl.cast::<IChannelAudioVolume>(), ctl.cast::<IAudioMeterInformation>()) else { continue };
                    let Ok(sid) = ctl.GetSessionIdentifier() else { continue };
                    let sid_str = sid.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(sid.0 as _));
                    // A session recreated while we had it ducked starts with the ducked channels (Windows persists them).
                    if self.pending_restore.remove(&sid_str) {
                        set_channels(&chan, 1.0);
                    }
                    if let Some(v) = self.legacy.remove(&sid_str) {
                        let _ = vol.SetMasterVolume(v, &CTX);
                        legacy_hit = true;
                    }
                    let root = root_pid(&procs, pid, name);
                    found.insert(key, Session { name: name.clone(), sid: sid_str, root, vol, chan, meter, last_set: None });
                }
            }
        }
        if legacy_hit {
            let path = self.state_path.with_file_name(LEGACY_STATE);
            let _ = if self.legacy.is_empty() { std::fs::remove_file(path) } else { std::fs::write(path, serde_json::to_string(&self.legacy).unwrap_or_default()) };
        }
        for s in self.sessions.values_mut() {
            if s.last_set.is_some() {
                self.pending_restore.insert(s.sid.clone()); // in case it comes back with the ducked channels
            }
            restore(s); // best effort: the session is usually gone already
        }
        self.sessions = found;

        self.watched = procs.iter().filter(|(_, (n, _))| watch(n)).map(|(&pid, (n, _))| (root_pid(&procs, pid, n), n.clone())).collect();
        let roots: HashSet<u32> = self.sessions.values().filter(|s| measure(&s.name)).map(|s| s.root).chain(self.watched.keys().copied()).collect();
        self.loopbacks.retain(|r, _| roots.contains(r));
        for r in roots {
            self.loopbacks.entry(r).or_insert_with(|| ProcessMeter::new(r).ok());
        }
    }

    pub fn peaks(&self) -> HashMap<String, f32> {
        let mut out: HashMap<String, f32> = HashMap::new();
        let mut read = HashSet::new();
        for (&root, name) in &self.watched {
            if let Some(Some(m)) = self.loopbacks.get(&root)
                && read.insert(root)
            {
                let e = out.entry(name.clone()).or_default();
                *e = e.max(m.peak().unwrap_or(0.0));
            }
        }
        for s in self.sessions.values() {
            let peak = match self.loopbacks.get(&s.root) {
                Some(Some(m)) if read.insert(s.root) => m.peak().unwrap_or(0.0),
                Some(Some(_)) => 0.0, // already drained this app's loopback this call
                Some(None) => unsafe { s.meter.GetPeakValue().unwrap_or(0.0) },
                None => 0.0,
            };
            let e = out.entry(s.name.clone()).or_default();
            *e = e.max(peak);
        }
        out
    }

    pub fn apply(&mut self, gains: &HashMap<String, f32>) {
        for s in self.sessions.values_mut() {
            let g = gains.get(&s.name).copied().unwrap_or(1.0);
            if s.last_set.is_none() && g >= 1.0 {
                continue; // not ducked and not about to be: no COM calls
            }
            if g >= 1.0 {
                restore(s);
                continue;
            }
            // Channel volumes are linear amplitude, the master volume `level` was tuned on is about squared (measured:
            // 0.5 gives -12 dB on master, -6 dB on channels), so square the gain to keep a duck as deep as it was.
            if s.last_set.is_none_or(|last| (g - last).abs() > 0.001) && set_channels(&s.chan, g * g) {
                s.last_set = Some(g);
            }
        }
        self.save_state();
    }

    pub fn restore_all(&mut self) {
        self.sessions.values_mut().for_each(restore);
        self.save_state();
    }

    /// User-facing volume of each app (ducking doesn't change it).
    pub fn volumes(&self) -> HashMap<String, f32> {
        self.sessions.values().filter_map(|s| Some((s.name.clone(), unsafe { s.vol.GetMasterVolume() }.ok()?))).collect()
    }

    fn save_state(&mut self) {
        let mut ducked: HashSet<String> = self.sessions.values().filter(|s| s.last_set.is_some()).map(|s| s.sid.clone()).collect();
        ducked.extend(self.pending_restore.iter().cloned());
        if ducked != self.saved {
            let _ = std::fs::write(&self.state_path, serde_json::to_string(&ducked).unwrap_or_default());
            self.saved = ducked;
        }
    }
}

fn restore(s: &mut Session) {
    if s.last_set.is_some() {
        set_channels(&s.chan, 1.0);
    }
    s.last_set = None;
}
