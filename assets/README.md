# Generated artwork

Everything in this directory was made for Nova: the posters, wallpaper and app icons with a
ComfyUI server running a Z-Image Turbo workflow (8 steps, the same workflow as
kernel/src/imagegen.rs), the symbolic glyphs with a small drawing script. The prompts asked for
original scenery, abstract art and plain symbols: no text, logos or people. `scripts/build.sh`
copies the files into the image under `usr/share/nova/`.

| Directory | What | Processing |
|---|---|---|
| `home/` | Home's posters and wallpaper, one file per Home item (`browser`, `terminal`, `web_video`, `power`, `wall`, ...) | 1024x576 (wallpaper 1280x800), resized to 640x360 |
| `icons/` | App icons (assistant, browser, terminal, files, editor, monitor, settings, power) | see below; 128x128 |
| `glyphs/` | Symbolic UI icons (48: arrows, search, grid/list, folder, document, gear, shield, cpu, memory, network, ...): white strokes on black, used as tint masks by `Canvas::draw_mask` | drawn, not generated: see below; 128x128 |

## App icons

One prompt template for the whole set, so every icon shares the same construction (a
rounded square tile, a two-step gradient lighter at the top, one white symbol, light from
above, no shadow outside the tile), like the icon grids of macOS and Windows 11 ask for.
Two seeds per icon were generated and the cleaner one kept. Post-processing (ImageMagick or
PIL): find the tile, crop it square, fill the black outside its rounded corners with the
tile's edge colour, resize to 128x128. The desktop rounds the corners itself (23% radius in
`icons.rs`), so all icons get exactly the same corner and the PNGs need no alpha channel.
They are checked at 128, 32, 24 and 16 px: the symbol must still read at 16.

## Symbolic glyphs

Image models do not draw matching strokes, so the symbolic set is drawn by a script:
128x128, every glyph fitted to the same 96 px keyline box, one stroke weight (14 px,
which is 2 px at the 16-24 px sizes they are shown at, like GNOME's and Fluent's symbolic
icons), round caps and joins, drawn at 4x and box-filtered down so the edges are
anti-aliased. `forward` and `back`, `up`, and the chevrons are the same arrow turned, so
pairs match exactly. The kernel tints them (`ui::symbol`) in the text colours, or draws
them white on a coloured tile (`ui::tile`) for sidebar sections.
