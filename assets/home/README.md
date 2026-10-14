# Home screen artwork

Posters and the wallpaper for Nova Home (`kernel/src/home.rs`). `scripts/build.sh` copies
them into the system files as `usr/share/nova/home/<name>.png`.

They were generated for Nova with a Z-Image Turbo workflow on a ComfyUI server (8 steps,
1024x576, wallpaper 1280x800), then resized to 640x360 (wallpaper 1280x800). The prompts
asked for original scenery and abstract art: no text, logos or people. Each file's name is
the Home item it belongs to (`assistant`, `browser`, `web_video`, `idea_game`, `wall`, ...);
items without a file get a procedural poster.
