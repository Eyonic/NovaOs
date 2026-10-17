//! Nova's media apps: Videos (video.rs), Music (music.rs) and Photos (photos.rs), the
//! library and the files they remember (library.rs), still-image decoding (image.rs),
//! the shared player chrome (chrome.rs) and the client of the libVLC engine process
//! (engine.rs). See docs/media.md.

pub mod chrome;
pub mod engine;
pub mod image;
pub mod library;
pub mod music;
pub mod photos;
pub mod video;

use alloc::format;
use alloc::string::{String, ToString};

/// Open `path` in the app for its kind (video, audio or picture). False if it is none.
pub fn open_file(path: &str) -> bool {
    if library::is_video(path) {
        video::open(Some(path.into()));
    } else if library::is_audio(path) {
        music::open(Some(path.into()));
    } else if image::is_image_name(path) {
        photos::open(Some(path.into()));
    } else {
        return false;
    }
    true
}

/// libVLC message level `media play` forwards to the log (0 errors, 1 warnings, 2 all).
static DEBUG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The `media` shell command (the test suite's hook): `media play <path> [seconds]` plays
/// through the engine without a window and reports the frames and audio seen; `media
/// decode <path>` decodes a picture and reports its size and centre pixel; `media probe
/// <path>` prints the tags the engine finds.
pub fn shell(args: &str) {
    // `media <verb> <path with spaces> [seconds]`
    let (verb, rest) = args.trim().split_once(' ').unwrap_or((args.trim(), ""));
    let rest = rest.trim();
    let (path, secs) = match rest.rsplit_once(' ') {
        Some((p, n)) if verb == "play" && n.parse::<u64>().is_ok() => (p.trim(), n.parse::<u64>().unwrap_or(3)),
        _ => (rest, 3),
    };
    let mut it = core::iter::once(secs.to_string());
    match (Some(verb), (!path.is_empty()).then_some(path)) {
        (Some("decode"), Some(path)) => match library::read(path).ok_or("no such file".into()).and_then(|d| image::decode(&d).map_err(String::from)) {
            Ok(img) => {
                let f = img.first();
                let px = f.pixels.get(f.height / 2 * f.width + f.width / 2).copied().unwrap_or(0);
                crate::println!("media: decoded {path}: {}x{} {} frame(s), centre #{px:06x}", f.width, f.height, img.frames.len());
            }
            Err(e) => crate::println!("media: {path}: {e}"),
        },
        (Some("engine-decode"), Some(path)) => match engine::Engine::start() {
            Ok(mut e) => match e.decode(path) {
                Some(f) => {
                    let px = f.pixels.get(f.height / 2 * f.width + f.width / 2).copied().unwrap_or(0);
                    crate::println!("media: engine decoded {path}: {}x{}, centre #{px:06x}", f.width, f.height);
                }
                None => crate::println!("media: the engine could not decode {path}: {}", e.error.clone().unwrap_or_default()),
            },
            Err(e) => crate::println!("media: {e}"),
        },
        (Some("probe"), Some(path)) => match engine::Engine::start() {
            Ok(mut e) => {
                for (k, v) in e.probe(path) {
                    crate::println!("  {k}: {v}");
                }
                crate::println!("media: probed {path}");
            }
            Err(e) => crate::println!("media: {e}"),
        },
        (Some("play"), Some(path)) => {
            let secs: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(3);
            let path = path.trim_start_matches('/');
            let mut e = match engine::Engine::start() {
                Ok(e) => e,
                Err(e) => {
                    crate::println!("media: {e}");
                    return;
                }
            };
            crate::println!("media: engine ready, playing {path} for {secs} s");
            e.set_debug(DEBUG.load(core::sync::atomic::Ordering::Relaxed));
            e.open(path, 0, false);
            let end = crate::pit::ticks() + secs * crate::pit::HZ;
            let mut first_frame = None;
            let mut last_state = engine::PlayState::Idle;
            while crate::pit::ticks() < end {
                if !e.pump() {
                    crate::println!("media: the engine stopped");
                    break;
                }
                if e.state != last_state {
                    last_state = e.state;
                    crate::println!("media: state {:?} at {} ms", e.state, e.position_ms());
                }
                if first_frame.is_none()
                    && let Some(f) = &e.frame
                {
                    let px = f.pixels[f.height / 2 * f.width + f.width / 2];
                    first_frame = Some(format!("{}x{} centre #{px:06x}", f.width, f.height));
                    crate::println!("media: first frame {}", first_frame.as_ref().unwrap());
                }
                if e.state == engine::PlayState::Ended {
                    break;
                }
                crate::ipc::wait(Some(crate::pit::ticks() + crate::pit::HZ / 50));
            }
            let tracks: alloc::vec::Vec<String> = e.tracks.iter().map(|t| format!("{}{}:{}", t.kind, t.id, t.name)).collect();
            crate::println!("media: played {path}: {} frames, {} ms of {} ms, state {:?}, audio {}, tracks [{}]", e.frames, e.position_ms(), e.length_ms, e.state, e.has_audio, tracks.join(" "));
            e.stop();
            // Let the audio drain through the mixer before the engine goes.
            crate::task::sleep_ticks(crate::pit::HZ / 2);
        }
        (Some("debug"), Some(level)) => DEBUG.store(level.parse().unwrap_or(0), core::sync::atomic::Ordering::Relaxed),
        _ => crate::println!("usage: media play <path> [seconds] | media decode <path> | media engine-decode <path> | media probe <path> | media debug 0|1|2"),
    }
}
