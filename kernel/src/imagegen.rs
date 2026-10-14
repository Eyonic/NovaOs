//! Image generation through a ComfyUI server (its HTTP API): the assistant uses it to make
//! artwork and assets for the apps it builds.
//!
//! The server address is not built in. scripts/run.sh passes it from the host
//! (`NOVA_COMFYUI_URL` or `~/.config/nova/comfyui-url`) as the fw_cfg file
//! `opt/nova/comfyui-url`; `nova.comfyui=<url>` on the kernel command line overrides it
//! (the tests' mock server). The workflow is Z-Image Turbo (8 steps), which needs these
//! models on the server: diffusion model `z_image_turbo_bf16.safetensors`, text encoder
//! `qwen_3_4b.safetensors` (lumina2) and VAE `ae.safetensors`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{json, Value};

use crate::{fwcfg, http, limine, pit, rand, task};

/// Base URL of the ComfyUI server, if one is configured.
pub fn server() -> Option<String> {
    let cmdline: &str = limine::EXECUTABLE_CMDLINE.get().map_or("", |c| c);
    if let Some(url) = cmdline.split_whitespace().find_map(|a| a.strip_prefix("nova.comfyui=")) {
        return Some(url.trim_end_matches('/').into());
    }
    static HOST: spin::Once<Option<String>> = spin::Once::new();
    HOST.call_once(|| {
        let data = fwcfg::read_file("opt/nova/comfyui-url")?;
        let url = core::str::from_utf8(&data).ok()?.trim().trim_end_matches('/');
        (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_string())
    })
    .clone()
}

fn workflow(prompt: &str, width: u32, height: u32, seed: u64) -> Value {
    json!({
        "1": {"class_type": "UNETLoader", "inputs": {"unet_name": "z_image_turbo_bf16.safetensors", "weight_dtype": "default"}},
        "2": {"class_type": "ModelSamplingAuraFlow", "inputs": {"model": ["1", 0], "shift": 3.0}},
        "3": {"class_type": "CLIPLoader", "inputs": {"clip_name": "qwen_3_4b.safetensors", "type": "lumina2", "device": "default"}},
        "4": {"class_type": "CLIPTextEncode", "inputs": {"text": prompt, "clip": ["3", 0]}},
        "5": {"class_type": "CLIPTextEncode", "inputs": {"text": "", "clip": ["3", 0]}},
        "6": {"class_type": "EmptySD3LatentImage", "inputs": {"width": width, "height": height, "batch_size": 1}},
        "7": {"class_type": "KSampler", "inputs": {"model": ["2", 0], "positive": ["4", 0], "negative": ["5", 0], "latent_image": ["6", 0],
              "seed": seed, "steps": 8, "cfg": 1.0, "sampler_name": "euler", "scheduler": "simple", "denoise": 1.0}},
        "8": {"class_type": "VAELoader", "inputs": {"vae_name": "ae.safetensors"}},
        "9": {"class_type": "VAEDecode", "inputs": {"samples": ["7", 0], "vae": ["8", 0]}},
        "10": {"class_type": "SaveImage", "inputs": {"images": ["9", 0], "filename_prefix": "nova"}},
    })
}

fn get_json(url: &str) -> Result<Value, String> {
    let r = http::request("GET", url, &[], &[])?.read_all()?;
    if r.status != 200 {
        return Err(format!("HTTP {} from {url}", r.status));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("bad JSON from {url}: {e}"))
}

fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Generate an image; returns the PNG file. Sizes are rounded to multiples of 16 and
/// limited to 64..=1024. Takes a few seconds to a minute, depending on the server.
pub fn generate(prompt: &str, width: u32, height: u32) -> Result<Vec<u8>, String> {
    let base = server().ok_or("no image server configured (set NOVA_COMFYUI_URL or ~/.config/nova/comfyui-url on the host)")?;
    let fit = |v: u32| (v.clamp(64, 1024) + 8) / 16 * 16;
    let (w, h) = (fit(width), fit(height));
    let mut seed = [0u8; 8];
    rand::fill(&mut seed);
    let body = json!({"prompt": workflow(prompt, w, h, u64::from_le_bytes(seed) >> 12)}).to_string();
    let r = http::request("POST", &format!("{base}/prompt"), &[("content-type", "application/json")], body.as_bytes())?.read_all()?;
    let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    if r.status != 200 {
        let msg = v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("request rejected");
        return Err(format!("image server: HTTP {}: {msg}", r.status));
    }
    let id = v.get("prompt_id").and_then(|p| p.as_str()).ok_or("image server gave no prompt_id")?.to_string();
    // Poll the history until the job is done (up to 5 minutes: the server may be loading
    // its models after a cold start, or busy with other jobs).
    for _ in 0..300 {
        task::sleep_ticks(pit::HZ);
        let hist = get_json(&format!("{base}/history/{id}"))?;
        let Some(job) = hist.get(&id) else { continue };
        if job.pointer("/status/status_str").and_then(|s| s.as_str()) == Some("error") {
            return Err("the image server reported an error for this job".into());
        }
        let image = job.get("outputs").and_then(|o| o.as_object()).and_then(|o| o.values().find_map(|n| n.get("images")?.as_array()?.first().cloned()));
        if let Some(img) = image {
            let s = |k: &str| img.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let url = format!("{base}/view?filename={}&subfolder={}&type={}", query_escape(&s("filename")), query_escape(&s("subfolder")), query_escape(&s("type")));
            let r = http::request("GET", &url, &[], &[])?.read_all()?;
            if r.status != 200 || !r.body.starts_with(b"\x89PNG") {
                return Err(format!("could not download the image (HTTP {})", r.status));
            }
            return Ok(r.body);
        }
    }
    Err("timed out waiting for the image server".into())
}
