# Generated artwork

Everything in this directory was made for Nova: the Home key art, wallpaper and app icons with a
ComfyUI server running a Z-Image Turbo workflow (8 steps, the same workflow as
kernel/src/imagegen.rs), the symbolic glyphs with a small drawing script. The prompts asked for
original scenery, abstract art and plain symbols: no text, logos or people. `scripts/build.sh`
copies the files into the image under `usr/share/nova/`.

| Directory | What | Processing |
|---|---|---|
| `home/` | Home's key art: a portrait capsule per item (`browser.png`, `web_video.png`, `restart.png`, ...), a wide hero per item (`hero_browser.png`, ...) and the wallpaper (`wall.png`) | see below; capsules 352x500, heroes 768x480, wallpaper 800x500 |
| `icons/` | App icons (assistant, browser, terminal, files, editor, monitor, settings, power) | see below; 128x128 |
| `glyphs/` | Symbolic UI icons (48: arrows, search, grid/list, folder, document, gear, shield, cpu, memory, network, ...): white strokes on black, used as tint masks by `Canvas::draw_mask` | drawn, not generated: see below; 128x128 |

## Home key art

Home shows every item twice: as a portrait card on the shelves (152x216, 176x250 when
focused, like a game capsule) and, when it is focused, as the backdrop of the whole screen
with the title, description and Open button over its left half. The first set was one
16:9 photo per item doing both jobs: the card saw only the middle 40% of it, the backdrop
was the same photo blown up, and the pictures were stock-photo clichés in unrelated styles
(a city at night for the browser, a fountain pen, brass gears, a green ECG line). Nothing
said Nova, and a shelf of them looked like a search results page.

The series was redone the way console and streaming libraries brief their key art (Steam's
library capsule and hero guidelines, the PS5 and Apple TV home screens, Netflix's artwork
research): one art direction for the whole set, one focal subject per piece, a signature
colour per item, legible at thumbnail size, with safe areas for the text the UI draws over
it, and separate capsule and hero compositions.

**Art direction: light sculptures.** Every item is a single object made of dark glass or
brushed metal that glows from within in the item's colour, floating in an empty black
studio, lit by one soft key light from the upper left, with a faint haze of its colour
behind it and a soft reflection on a dark glossy floor. The material, light and stage are
the same for all; only the object and its colour change. That is what gives a shelf its
family resemblance, and it matches the icons (`icons/`): flat gradient tiles in the same
colours, one symbol each. The posters are the icons' subjects turned into objects.
Nothing is a stock scene, no text, logos, trademarks or people.

| Item | Colour | Object (the Nova motif) |
|---|---|---|
| Browser | orange-red (icon `#FB542B`) | a smoky glass globe etched with glowing lines of latitude, one bright ring orbiting it |
| Terminal | teal-cyan on slate | a dark obsidian monolith with a right-pointing chevron of cyan light cut into it, a cursor bar at its base |
| Text Editor | violet | one calligraphic ribbon of violet light, tapering to a nib |
| Files | sky blue | a fanned stack of thin blue glass sheets, edge-lit |
| System Monitor | green | concentric rings of green light rippling on black water: a pulse, not an ECG |
| Settings | brushed silver | a gyroscope of three nested machined rings |
| Restart | violet | a ring of violet light, open at one point, its end sweeping into an arrow |
| Shut down | deep violet | a dark glass sphere with a last ember glowing at its core |
| YouTube (`web_video`) | crimson | a red crystal wedge projecting a cone of light |
| Wikipedia (`web_wiki`) | cool white | a globe made of thousands of stacked pages |
| GitHub (`web_code`) | indigo | a constellation of glass nodes joined by lines of light (a commit graph) |
| DuckDuckGo (`web_search`) | amber | a glass lens focusing light to one bright point |
| Hacker News (`web_news`) | golden amber | a half disc of light rising over a flat horizon |
| Rust (`web_forge`) | molten rust-orange | a bar of forged metal still glowing from the forge |
| Wallpaper (`wall`) | violet on near-black | a single small star flaring in a dark indigo sky, the nova |

**Composition rules.**

- Capsule (`<id>.png`, 352x500 = the focused card at 2x, 0.704 ratio): the object is
  centred and fills about two thirds of the width in the upper two thirds of the frame;
  the bottom quarter is quiet, because `home.rs` draws a scrim and the title there.
- Hero (`hero_<id>.png`, 768x480, the 16:10 of the screen; `home.rs` enlarges it
  bilinearly under its scrims, so it needs no more pixels): the object is about a third of
  the frame high and sits at two thirds of the width; the left half is empty black (the
  hero text, the category chip and the Open button sit in the left 55%, and `home.rs` adds
  a gradient to the UI background there plus two fades down to the shelves). Renders come
  out more centred than asked, so the hero is a 16:10 window of a wider 1.75:1 render,
  placed by the subject's brightness centroid.
- The background is a true black studio, so the shelf's dark UI (`#0B0C12`) and the art
  blend without visible edges, and the card's rounded corners cut nothing important.
- One object, no secondary props, no horizon except where it is the subject (news).
- Checked at 152x216 (card), 1280x800 under the backdrop scrim, and on the shelf as a
  row: the row must read as one set of different colours, not as unrelated photos.

**Generation.** ComfyUI, Z-Image Turbo, 8 steps, cfg 1 (the workflow of
`kernel/src/imagegen.rs`); capsules rendered at 896x1280, heroes at 1792x1024, the wallpaper at
1536x960; two or more seeds per piece, the best picked by eye at card size, then cropped
and Lanczos-resized with PIL and saved as 8-bit RGB PNG (the kernel's decoder reads 8-bit
non-interlaced PNG only). The prompt template is

> *\<object\>*. A minimalist 3D product render, the object floating in an empty
> pitch-black studio, one soft key light from the upper left, a faint *\<colour\>* haze
> glowing behind it, a soft reflection on the dark glossy floor, volumetric light, ultra
> clean, photoreal octane render, no other objects, no text, no letters, no people.
> Composition: *capsule:* tall portrait frame, the object centered horizontally in the upper
> two thirds of the frame, filling about 65% of the width, the bottom quarter of the frame
> is empty dark space. *hero:* very wide cinematic frame, the object is small, about a third
> of the frame height, placed right of centre at two thirds of the width, the whole left
> half of the frame is empty dark space with only faint haze, lots of negative space, the
> object fully inside the frame.

To add an item, write one line for its object in the table's language, keep the template,
and give it a colour that is not yet on the shelf.

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

## Craft App Suite icons and placeholder art (third-party)

`icons/craft_<app>.png`, `home/craft_<app>.png` and `home/hero_craft_<app>.png` are not
generated for Nova: they are made by `scripts/make-craft-art.py` from each app's own icon
(`assets/app-icon/hicolor/256x256/apps/ai.storyteller.<app>.png` in its repository) for the
Craft App Suite by storytold: WordCraft, GridCraft, DeckCraft, PdfCraft, PhotoCraft,
LightCraft, FilmCraft, SoundCraft, VectorCraft, DesignCraft, FigCraft, CadCraft,
EffectCraft and the ArtCraft launcher (https://github.com/storytold/<app>), licensed
MIT OR Apache-2.0, Copyright (c) their authors. The icons are used as the apps' icons for
the apps themselves (installed with `NOVA_CRAFT=...`, see docs/craft-apps.md); the script
only fills the transparent corners with the tile colour and resizes (icons), or places the
icon on a black stage with a haze of that colour and a floor reflection (capsules 352x500,
heroes 768x480, in the layout of Home's key art above). They are placeholders until the
apps get key art in the series' direction.
