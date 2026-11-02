#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! AudioDuck runs as a Stream Deck / OpenDeck plugin: no window, no tray. Each key is set up in its own property
//! inspector (streamdeck/…/ui/rule.html) and its settings are what it does; tapping the key turns it off and on.
mod audio;
mod engine;
mod icon;
mod keyimage;
mod streamdeck;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine::{Ducker, Rule, Settings};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

const TICK: Duration = Duration::from_millis(30);
const REFRESH: Duration = Duration::from_secs(1);

/// Shared by all keys: threshold and timings.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
}

/// What the engine measured. Per-app values are only filled while a property inspector is open.
#[derive(Default, Serialize)]
pub struct Live {
    pub peaks: Vec<(String, f32)>,
    pub gains: HashMap<String, f32>,
    pub volumes: HashMap<String, f32>,
    /// Key setups ducking right now.
    #[serde(skip)]
    pub active: Vec<Rule>,
}

pub struct Shared {
    pub config: Config,
    /// One per AudioDuck key, from the keys' own settings (see streamdeck.rs).
    pub rules: Vec<Rule>,
    pub rev: u64, // bumped on every config change so the engine picks it up
    pub live: Live,
    pub inspector_open: bool,
    pub quit: bool,
}

pub type SharedRef = Arc<Mutex<Shared>>;

fn app_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("AudioDuck")
}

// config.json belongs to the desktop app (rules and all); the plugin only reads its timings until it has saved its own.
const CONFIG: &str = "plugin.json";

fn load_config() -> Config {
    [CONFIG, "config.json"].iter().find_map(|f| std::fs::read_to_string(app_dir().join(f)).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_config(c: &Config) {
    let _ = std::fs::create_dir_all(app_dir());
    let _ = std::fs::write(app_dir().join(CONFIG), serde_json::to_string_pretty(c).unwrap_or_default());
}

/// Audio loop. Owns every COM object; talks to the plugin side only through `Shared`.
fn engine(shared: SharedRef) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut mixer = audio::Mixer::new(app_dir().join("ducked_channels.json"));
    let (mut cfg, mut rules) = { let sh = shared.lock().unwrap(); (sh.config.clone(), sh.rules.clone()) };
    let mut ducker = Ducker::default();
    let mut rev = u64::MAX;
    let (mut last_tick, mut last_refresh) = (Instant::now(), Instant::now() - REFRESH);
    let mut was_open = false;

    loop {
        std::thread::sleep(TICK);
        let now = Instant::now();
        let dt = (now - last_tick).as_secs_f64().min(0.2) * 1000.0;
        last_tick = now;
        let open = {
            let sh = shared.lock().unwrap();
            if sh.quit {
                mixer.restore_all();
                return;
            }
            if sh.rev != rev {
                rev = sh.rev;
                if sh.rules != rules {
                    ducker.rules_changed();
                    last_refresh -= REFRESH; // measure new sources right away
                    rules = sh.rules.clone();
                }
                cfg = sh.config.clone();
            }
            sh.inspector_open
        };
        if open != was_open {
            was_open = open;
            last_refresh -= REFRESH;
        }
        if now - last_refresh >= REFRESH {
            last_refresh = now;
            // Inspector closed: only rule sources need accurate levels. Open: every app, for the meters.
            let source = |app: &str| rules.iter().any(|r| r.enabled && r.sources.iter().any(|s| s == app));
            mixer.refresh(|app| open || source(app), source);
        }
        let peaks = mixer.peaks();
        mixer.apply(ducker.step(&rules, &cfg.settings, &peaks, dt));
        let active = rules.iter().zip(&ducker.active).filter(|&(_, &a)| a).map(|(r, _)| r.clone()).collect();
        let mut sh = shared.lock().unwrap();
        sh.live.active = active;
        if open {
            let mut list: Vec<_> = peaks.into_iter().collect();
            list.sort_by(|a, b| a.0.cmp(&b.0));
            sh.live.peaks = list;
            sh.live.gains = ducker.gains.clone();
            sh.live.volumes = mixer.volumes();
        }
    }
}

fn main() {
    let Some(plugin) = streamdeck::Args::parse() else { return }; // only runs when started by Stream Deck / OpenDeck
    // Two instances would each take the other's ducked volume as the user's and push it down for good.
    let _single = unsafe { CreateMutexW(None, false, w!("AudioDuck.SingleInstance")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    let config = load_config();
    let shared: SharedRef = Arc::new(Mutex::new(Shared { config, rules: vec![], rev: 0, live: Live::default(), inspector_open: false, quit: false }));
    let sd = shared.clone();
    std::thread::spawn(move || {
        streamdeck::run(plugin, &sd);
        sd.lock().unwrap().quit = true; // Stream Deck is gone: restore volumes and exit with it
    });
    engine(shared);
}
