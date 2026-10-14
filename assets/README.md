# Generated artwork

Everything in this directory was generated for Nova with a ComfyUI server running a
Z-Image Turbo workflow (8 steps), then post-processed with ImageMagick. The prompts asked for
original scenery, abstract art and plain symbols: no text, logos or people. `scripts/build.sh`
copies the files into the image under `usr/share/nova/`.

| Directory | What | Processing |
|---|---|---|
| `home/` | Home's posters and wallpaper, one file per Home item (`assistant`, `browser`, `web_video`, `idea_game`, `wall`, ...) | 1024x576 (wallpaper 1280x800), resized to 640x360 |
| `icons/` | App icons (assistant, browser, terminal, files, editor, monitor, settings, power): glossy rounded tiles, generated as one set in a shared prompt style | trimmed to the tile, 128x128; corners are rounded when drawn |
| `glyphs/` | Toolbar glyphs (back, forward, reload, home, lock, shield): white strokes on black, used as tint masks | trimmed and fitted to the same 96 px box on 128x128; `forward` is `back` mirrored, so the pair matches exactly; the lock was generated solid and turned into an outline of the same stroke weight |

Image models do not draw the same thing twice, so symbols that must match (arrow pairs) come
from one image, and every new icon is checked against the set before it is used. Items
without artwork get a procedural poster or tile.
