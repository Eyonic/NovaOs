#!/usr/bin/env bash
# Generate sample media for the image with ffmpeg (nothing binary lives in the repository):
# short test-pattern videos with a sine tone, tagged songs with cover art, and pictures in
# every format the Image Viewer decodes. The players list them as the "Samples" folder.
#   scripts/make-test-media.sh <output directory>
set -euo pipefail
out="${1:?usage: make-test-media.sh <directory>}"
mkdir -p "$out"
command -v ffmpeg >/dev/null || { echo "make-test-media: ffmpeg not found" >&2; exit 1; }
ff() { ffmpeg -nostdin -loglevel error -y "$@"; }

# A cover picture: a gradient with a ring (also the WebP/GIF/JPEG test pictures below).
ff -f lavfi -i "gradients=size=640x640:c0=#1B1C27:c1=#E0559A:x0=0:y0=0:x1=640:y1=640:duration=1" -frames:v 1 "$out/cover.png"
ff -f lavfi -i "testsrc2=size=800x500:rate=1" -frames:v 1 -update 1 "$out/pattern.png"
ff -i "$out/pattern.png" -q:v 3 "$out/pattern.jpg"
# A JPEG stored on its side whose EXIF orientation says "rotate 90 clockwise": the viewer
# must show it upright (landscape), as cameras expect. Needs Python's PIL; skipped without.
python3 -I - "$out" <<'PY' 2>/dev/null || true
import sys
from PIL import Image
out = sys.argv[1]
im = Image.open(out + "/pattern.png").convert("RGB").transpose(Image.ROTATE_90)
exif = Image.Exif()
exif[0x0112] = 6
im.save(out + "/rotated.jpg", quality=85, exif=exif)
PY
ff -i "$out/pattern.png" -c:v libwebp -quality 80 "$out/pattern.webp"
ff -f lavfi -t 2 -i "testsrc2=size=320x200:rate=10" -vf "fps=10,scale=320:-1:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=64[p];[s1][p]paletteuse" "$out/animation.gif"

# Videos: H.264 + AAC in MP4, VP9 + Opus in WebM, H.264 + FLAC in MKV (lossless audio, so
# the audio test can check the tone exactly). A subtitle file goes with the first one.
ff -f lavfi -i "testsrc2=size=640x360:rate=30" -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 6 \
   -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a aac -b:a 128k -metadata title="Test Pattern" "$out/Test Pattern.mp4"
ff -f lavfi -i "testsrc2=size=640x360:rate=30" -f lavfi -i "sine=frequency=660:sample_rate=48000" -t 6 \
   -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -b:v 600k -pix_fmt yuv420p -c:a libopus -b:a 96k -metadata title="Colour Bars" "$out/Colour Bars.webm"
ff -f lavfi -i "smptebars=size=640x360:rate=25" -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 5 \
   -c:v libx264 -preset veryfast -pix_fmt yuv420p -c:a flac -metadata title="Tone Check" "$out/Tone Check.mkv"
printf '1\n00:00:00,500 --> 00:00:03,000\nNova plays subtitles too.\n\n2\n00:00:03,200 --> 00:00:05,800\nThis line came from an .srt file.\n' > "$out/Test Pattern.srt"

# Songs with tags and embedded cover art: MP3, FLAC, Ogg Vorbis, Opus.
tone() { # tone <file> <hz> <seconds> <title> <track> <extra args...>
    local f="$1" hz="$2" s="$3" title="$4" track="$5"; shift 5
    ff -f lavfi -i "sine=frequency=$hz:sample_rate=44100" -f lavfi -i "sine=frequency=$((hz * 3 / 2)):sample_rate=44100" -i "$out/cover.png" \
       -filter_complex "[0:a][1:a]amix=inputs=2:weights=1 0.3,volume=0.6,afade=t=in:d=0.2,afade=t=out:st=$((s - 1)):d=1[a]" -map "[a]" -map 2:v -t "$s" \
       -metadata title="$title" -metadata artist="Nova Test Tones" -metadata album="Sine Waves" -metadata track="$track" -metadata date=2026 \
       -disposition:v attached_pic "$@" "$f"
}
tone "$out/01 Concert A.mp3" 440 8 "Concert A" 1 -c:a libmp3lame -q:a 4 -c:v mjpeg -id3v2_version 3
tone "$out/02 Fifth Above.flac" 660 8 "Fifth Above" 2 -c:a flac -c:v png
ff -f lavfi -i "sine=frequency=330:sample_rate=48000" -t 8 -c:a libvorbis -q:a 4 -metadata title="Low E" -metadata artist="Nova Test Tones" -metadata album="Sine Waves" -metadata track=3 "$out/03 Low E.ogg"
ff -f lavfi -i "sine=frequency=880:sample_rate=48000" -t 8 -c:a libopus -b:a 96k -metadata title="High A" -metadata artist="Another Artist" -metadata album="Opus Tests" -metadata track=1 "$out/04 High A.opus"
rm -f "$out/cover.png"
echo "make-test-media: $(ls "$out" | wc -l) files, $(du -sh "$out" | cut -f1)"
