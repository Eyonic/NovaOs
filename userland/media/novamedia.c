/* novamedia: the decoding engine behind Nova's Video Player, Music Player and Image
 * Viewer. A small Linux program that links libVLC (LGPL 2.1; copied from the build host
 * into the image only, see scripts/build.sh and docs/media.md) and talks to the native
 * app in the kernel over two pipes:
 *
 *   fd 3  commands in, one per line:
 *         open <path> [<start-ms>]   load and start playing (paused with "open-paused")
 *         play | pause | toggle | stop | quit
 *         seek <ms>                   absolute position
 *         rate <percent>              playback speed (25..400)
 *         volume <0..200> | mute <0|1>
 *         atrack <id> | strack <id> | vtrack <id>
 *         adelay <ms> | sdelay <ms>   audio / subtitle delay
 *         sub <path>                  add an external subtitle file and select it
 *         eq <preset>                 equalizer preset index, -1 = off
 *         probe <path>                metadata and tracks only (no playback)
 *         thumb <path> <percent>      decode one frame at that position into the frame buffer
 *         decode <path>               decode an image file into the frame buffer
 *   fd 4  events out, one per line:
 *         ready                       libVLC is up
 *         shm <fd> <bytes>            the frame buffer: a memfd on that descriptor
 *         format <w> <h> <pitch> <slotbytes>
 *         frame <slot> <w> <h>        a frame was rendered into that slot
 *         time <ms>, length <ms>, state <opening|playing|paused|stopped|ended|error>
 *         meta <key> <value>          title, artist, album, genre, date, track, artwork
 *         track <a|s|v> <id> <name>   one selectable track (id -1 = disabled), tracks-end
 *         video <w> <h>, done <what>, error <text>
 *
 * Frames are RV32 (BGRA in memory, which the kernel reads as 0xRRGGBB words) in two slots
 * of the shared buffer: libVLC renders into one while the kernel copies the other. The
 * kernel reads the pages of the memfd directly (linux::shared_memory), so nothing is
 * copied through the pipe. Audio goes through libVLC's pulse module to Nova's in-kernel
 * PulseAudio server; no window system is involved.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <vlc/vlc.h>

#define CMD_FD 3
#define EVT_FD 4
#define SHM_FD 5
#define MAX_W 1920
#define MAX_H 1088
#define SLOT_BYTES ((size_t)MAX_W * MAX_H * 4)

static libvlc_instance_t *vlc;
static libvlc_media_player_t *mp;
static libvlc_media_t *media;
static libvlc_equalizer_t *eq;
static pthread_mutex_t out_lock = PTHREAD_MUTEX_INITIALIZER;
static uint8_t *shm;
static int slot;                  /* slot libVLC renders into next */
static unsigned vw, vh, vpitch;   /* current video format */
static volatile int one_frame;    /* thumb/decode: stop after the first displayed frame */
static volatile int frames;
static int64_t last_time = -1;
static int log_level = 1000; /* libVLC messages at this level or above become `log` events (off until `debug`) */

static void say(const char *fmt, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf - 1, fmt, ap);
    va_end(ap);
    if (n < 0) return;
    if (n > (int)sizeof buf - 2) n = sizeof buf - 2;
    buf[n++] = '\n';
    pthread_mutex_lock(&out_lock);
    const char *p = buf;
    while (n > 0) {
        ssize_t w = write(EVT_FD, p, n);
        if (w <= 0) { if (errno == EINTR) continue; break; }
        p += w; n -= w;
    }
    pthread_mutex_unlock(&out_lock);
}

/* Values go on the wire with newlines and control characters replaced. */
static const char *clean(const char *s, char *buf, size_t n) {
    if (!s) return "";
    size_t i = 0;
    for (; *s && i + 1 < n; s++) buf[i++] = ((unsigned char)*s < 32) ? ' ' : *s;
    buf[i] = 0;
    return buf;
}

/* ---- video output into the shared buffer ------------------------------------- */

static unsigned format_cb(void **opaque, char *chroma, unsigned *w, unsigned *h, unsigned *pitches, unsigned *lines) {
    (void)opaque;
    memcpy(chroma, "RV32", 4);
    /* Keep the source size (the kernel scales), capped to what the buffer holds. */
    if (*w > MAX_W) { *h = *h * MAX_W / *w; *w = MAX_W; }
    if (*h > MAX_H) { *w = *w * MAX_H / *h; *h = MAX_H; }
    if (*w == 0) *w = 1;
    if (*h == 0) *h = 1;
    vw = *w; vh = *h;
    vpitch = (*w * 4 + 31) & ~31u;
    pitches[0] = vpitch;
    lines[0] = (*h + 31) & ~31u;
    if ((size_t)vpitch * lines[0] > SLOT_BYTES) { vh = *h = SLOT_BYTES / vpitch - 32; lines[0] = (*h + 31) & ~31u; }
    say("format %u %u %u %zu", vw, vh, vpitch, SLOT_BYTES);
    return 1;
}

static void *lock_cb(void *opaque, void **planes) {
    (void)opaque;
    planes[0] = shm + (size_t)slot * SLOT_BYTES;
    return (void *)(intptr_t)slot;
}

static void unlock_cb(void *opaque, void *picture, void *const *planes) { (void)opaque; (void)picture; (void)planes; }

static void display_cb(void *opaque, void *picture) {
    (void)opaque;
    int s = (int)(intptr_t)picture;
    frames++;
    if (one_frame && frames > 1) return; /* one is enough: the main loop stops the player */
    say("frame %d %u %u", s, vw, vh);
    slot = s ^ 1;
}

/* ---- events ------------------------------------------------------------------- */

static void send_tracks(void) {
    libvlc_track_description_t *t, *d;
    for (int kind = 0; kind < 3; kind++) {
        char k = "asv"[kind];
        d = kind == 0 ? libvlc_audio_get_track_description(mp) : kind == 1 ? libvlc_video_get_spu_description(mp) : libvlc_video_get_track_description(mp);
        char buf[256];
        for (t = d; t; t = t->p_next) say("track %c %d %s", k, t->i_id, clean(t->psz_name, buf, sizeof buf));
        libvlc_track_description_list_release(d);
    }
    say("tracks-end %d %d", libvlc_audio_get_track(mp), libvlc_video_get_spu(mp));
}

static void send_meta(libvlc_media_t *m) {
    static const struct { libvlc_meta_t id; const char *name; } keys[] = {
        {libvlc_meta_Title, "title"}, {libvlc_meta_Artist, "artist"}, {libvlc_meta_Album, "album"},
        {libvlc_meta_Genre, "genre"}, {libvlc_meta_Date, "date"}, {libvlc_meta_TrackNumber, "track"},
        {libvlc_meta_ArtworkURL, "artwork"}, {libvlc_meta_AlbumArtist, "albumartist"},
    };
    char buf[512];
    for (size_t i = 0; i < sizeof keys / sizeof keys[0]; i++) {
        char *v = libvlc_media_get_meta(m, keys[i].id);
        if (v && *v) say("meta %s %s", keys[i].name, clean(v, buf, sizeof buf));
        free(v);
    }
    libvlc_media_track_t **tracks;
    unsigned n = libvlc_media_tracks_get(m, &tracks);
    for (unsigned i = 0; i < n; i++) {
        if (tracks[i]->i_type == libvlc_track_video && tracks[i]->video)
            say("video %u %u", tracks[i]->video->i_width, tracks[i]->video->i_height);
        if (tracks[i]->i_type == libvlc_track_audio) say("hasaudio");
    }
    if (n) libvlc_media_tracks_release(tracks, n);
    say("length %lld", (long long)libvlc_media_get_duration(m));
}

static void on_event(const libvlc_event_t *e, void *data) {
    (void)data;
    switch (e->type) {
    case libvlc_MediaPlayerTimeChanged: {
        int64_t t = e->u.media_player_time_changed.new_time;
        if (last_time < 0 || t / 250 != last_time / 250) { last_time = t; say("time %lld", (long long)t); }
        break;
    }
    case libvlc_MediaPlayerLengthChanged: say("length %lld", (long long)e->u.media_player_length_changed.new_length); break;
    case libvlc_MediaPlayerPlaying: say("state playing"); break;
    case libvlc_MediaPlayerPaused: say("state paused"); break;
    case libvlc_MediaPlayerStopped: say("state stopped"); break;
    case libvlc_MediaPlayerEndReached: say("state ended"); break;
    case libvlc_MediaPlayerEncounteredError: say("state error"); break;
    case libvlc_MediaPlayerOpening: say("state opening"); break;
    case libvlc_MediaPlayerESAdded: case libvlc_MediaPlayerESDeleted: case libvlc_MediaPlayerVout: say("tracks-changed"); break;
    case libvlc_MediaParsedChanged: say("parsed"); break;
    default: break;
    }
}

static void attach(void) {
    libvlc_event_manager_t *em = libvlc_media_player_event_manager(mp);
    static const libvlc_event_type_t evs[] = {
        libvlc_MediaPlayerTimeChanged, libvlc_MediaPlayerLengthChanged, libvlc_MediaPlayerPlaying, libvlc_MediaPlayerPaused,
        libvlc_MediaPlayerStopped, libvlc_MediaPlayerEndReached, libvlc_MediaPlayerEncounteredError, libvlc_MediaPlayerOpening,
        libvlc_MediaPlayerESAdded, libvlc_MediaPlayerESDeleted, libvlc_MediaPlayerVout,
    };
    for (size_t i = 0; i < sizeof evs / sizeof evs[0]; i++) libvlc_event_attach(em, evs[i], on_event, NULL);
}

static FILE *logf; /* `debug 2`: every message goes to /run/novamedia.log as well */

static void log_cb(void *data, int level, const libvlc_log_t *ctx, const char *fmt, va_list args) {
    (void)data;
    if (level < log_level && !logf) return;
    const char *module = "vlc", *file; unsigned line;
    libvlc_log_get_context(ctx, &module, &file, &line);
    char msg[512];
    vsnprintf(msg, sizeof msg, fmt, args);
    const char *kind = level >= LIBVLC_ERROR ? "error" : level >= LIBVLC_WARNING ? "warning" : "debug";
    if (logf) fprintf(logf, "%s %s: %s\n", kind, module, msg);
    if (level < log_level) return;
    char buf[512];
    say("log %s %s: %s", kind, module, clean(msg, buf, sizeof buf));
}

/* ---- commands ----------------------------------------------------------------- */

static void set_media(const char *path, int64_t start_ms, int audio) {
    if (media) { libvlc_media_release(media); media = NULL; }
    media = libvlc_media_new_path(vlc, path);
    if (!media) { say("error cannot open %s", path); return; }
    char opt[64];
    if (start_ms > 0) { snprintf(opt, sizeof opt, ":start-time=%lld.%03lld", (long long)(start_ms / 1000), (long long)(start_ms % 1000)); libvlc_media_add_option(media, opt); }
    if (!audio) libvlc_media_add_option(media, ":no-audio");
    libvlc_media_player_set_media(mp, media);
}

/* Parse a file's metadata synchronously (local only: no network fetching). */
static void probe(const char *path) {
    libvlc_media_t *m = libvlc_media_new_path(vlc, path);
    if (!m) { say("error cannot open %s", path); return; }
    libvlc_media_parse_with_options(m, libvlc_media_parse_local | libvlc_media_fetch_local, 3000);
    for (int i = 0; i < 300 && libvlc_media_get_parsed_status(m) == 0; i++) usleep(10000);
    send_meta(m);
    libvlc_media_release(m);
    say("done probe");
}

static void command(char *line) {
    char *arg = strchr(line, ' ');
    if (arg) *arg++ = 0; else arg = "";
    if (!strcmp(line, "open") || !strcmp(line, "open-paused")) {
        char *sp = strrchr(arg, ' ');
        int64_t start = 0;
        if (sp && sp != arg && sp[1] >= '0' && sp[1] <= '9') { start = atoll(sp + 1); *sp = 0; }
        one_frame = 0; frames = 0; last_time = -1;
        set_media(arg, start, 1);
        if (!media) return;
        libvlc_media_player_play(mp);
        if (!strcmp(line, "open-paused")) libvlc_media_player_set_pause(mp, 1);
    } else if (!strcmp(line, "thumb") || !strcmp(line, "decode")) {
        char *sp = strrchr(arg, ' ');
        int percent = 0;
        if (!strcmp(line, "thumb") && sp) { percent = atoi(sp + 1); *sp = 0; }
        libvlc_media_player_stop(mp);
        one_frame = 1; frames = 0;
        set_media(arg, 0, 0);
        if (!media) return;
        if (percent > 0) {
            libvlc_media_parse_with_options(media, libvlc_media_parse_local, 2000);
            for (int i = 0; i < 200 && libvlc_media_get_parsed_status(media) == 0; i++) usleep(10000);
            int64_t len = libvlc_media_get_duration(media);
            if (len > 0) {
                char opt[64];
                int64_t ms = len * percent / 100;
                snprintf(opt, sizeof opt, ":start-time=%lld.%03lld", (long long)(ms / 1000), (long long)(ms % 1000));
                libvlc_media_add_option(media, opt);
            }
        }
        libvlc_media_player_play(mp);
        /* Wait for the first frame (or give up), then stop: the frame stays in its slot. */
        for (int i = 0; i < 600 && frames == 0; i++) {
            usleep(10000);
            libvlc_state_t s = libvlc_media_player_get_state(mp);
            if (s == libvlc_Ended || s == libvlc_Error) break;
        }
        libvlc_media_player_stop(mp);
        if (frames == 0) say("error no frame in %s", arg);
        if (!strcmp(line, "thumb")) send_meta(media);
        say("done %s", line);
        one_frame = 0;
    } else if (!strcmp(line, "probe")) probe(arg);
    else if (!strcmp(line, "play")) libvlc_media_player_play(mp);
    else if (!strcmp(line, "pause")) libvlc_media_player_set_pause(mp, 1);
    else if (!strcmp(line, "toggle")) {
        if (libvlc_media_player_get_state(mp) == libvlc_Ended) libvlc_media_player_play(mp); else libvlc_media_player_pause(mp);
    } else if (!strcmp(line, "stop")) libvlc_media_player_stop(mp);
    else if (!strcmp(line, "seek")) {
        libvlc_media_player_set_time(mp, atoll(arg));
        say("time %lld", atoll(arg));
    } else if (!strcmp(line, "rate")) libvlc_media_player_set_rate(mp, atoi(arg) / 100.0f);
    else if (!strcmp(line, "volume")) libvlc_audio_set_volume(mp, atoi(arg));
    else if (!strcmp(line, "mute")) libvlc_audio_set_mute(mp, atoi(arg));
    else if (!strcmp(line, "atrack")) { libvlc_audio_set_track(mp, atoi(arg)); send_tracks(); }
    else if (!strcmp(line, "strack")) { libvlc_video_set_spu(mp, atoi(arg)); send_tracks(); }
    else if (!strcmp(line, "vtrack")) { libvlc_video_set_track(mp, atoi(arg)); send_tracks(); }
    else if (!strcmp(line, "adelay")) libvlc_audio_set_delay(mp, atoll(arg) * 1000);
    else if (!strcmp(line, "sdelay")) libvlc_video_set_spu_delay(mp, atoll(arg) * 1000);
    else if (!strcmp(line, "sub")) {
        char url[1200];
        snprintf(url, sizeof url, "file://%s", arg);
        if (libvlc_media_player_add_slave(mp, libvlc_media_slave_type_subtitle, url, true) != 0) say("error cannot add subtitle %s", arg);
        usleep(200000);
        send_tracks();
    } else if (!strcmp(line, "tracks")) send_tracks();
    else if (!strcmp(line, "meta")) { if (media) send_meta(media); }
    else if (!strcmp(line, "eq")) {
        int preset = atoi(arg);
        if (eq) { libvlc_audio_equalizer_release(eq); eq = NULL; }
        if (preset >= 0 && (unsigned)preset < libvlc_audio_equalizer_get_preset_count()) eq = libvlc_audio_equalizer_new_from_preset(preset);
        libvlc_media_player_set_equalizer(mp, eq);
    } else if (!strcmp(line, "presets")) {
        unsigned n = libvlc_audio_equalizer_get_preset_count();
        for (unsigned i = 0; i < n; i++) say("preset %u %s", i, libvlc_audio_equalizer_get_preset_name(i));
        say("presets-end");
    } else if (!strcmp(line, "debug")) {
        log_level = atoi(arg) >= 1 ? LIBVLC_WARNING : 1000;
        if (atoi(arg) >= 2 && !logf && (logf = fopen("/run/novamedia.log", "w"))) setvbuf(logf, NULL, _IOLBF, 0);
    } else if (!strcmp(line, "sleep")) {
        usleep(atoi(arg) * 1000);
    } else if (!strcmp(line, "quit")) {
        libvlc_media_player_stop(mp);
        exit(0);
    } else say("error unknown command %s", line);
}

int main(int argc, char **argv) {
    (void)argc; (void)argv;
    setenv("VLC_PLUGIN_PATH", "/usr/lib/vlc/plugins", 0);
    setenv("PULSE_SERVER", "unix:/run/user/1000/pulse/native", 0);
    setenv("HOME", "/home/nova", 0);
    mkdir("/home/nova", 0755);
    int fd = memfd_create("nova-media-frames", 0);
    if (fd < 0 || ftruncate(fd, 2 * SLOT_BYTES) < 0) { say("error memfd: %s", strerror(errno)); return 1; }
    if (fd != SHM_FD) { dup2(fd, SHM_FD); close(fd); fd = SHM_FD; }
    shm = mmap(NULL, 2 * SLOT_BYTES, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (shm == MAP_FAILED) { say("error mmap: %s", strerror(errno)); return 1; }
    const char *args[] = {
        "--no-xlib", "--aout=pulse", "--vout=vmem", "--no-video-title-show", "--no-osd", "--no-stats", "--no-lua",
        "--no-media-library", "--no-one-instance", "--file-caching=600", "--no-snapshot-preview", "--subsdec-encoding=UTF-8",
    };
    vlc = libvlc_new(sizeof args / sizeof args[0], args);
    if (!vlc) { say("error libvlc_new failed (plugins missing?)"); return 1; }
    libvlc_log_set(vlc, log_cb, NULL);
    mp = libvlc_media_player_new(vlc);
    libvlc_video_set_format_callbacks(mp, format_cb, NULL);
    libvlc_video_set_callbacks(mp, lock_cb, unlock_cb, display_cb, NULL);
    attach();
    say("shm %d %zu", SHM_FD, 2 * SLOT_BYTES);
    say("ready %s", libvlc_get_version());

    char buf[4096];
    size_t len = 0;
    for (;;) {
        ssize_t n = read(CMD_FD, buf + len, sizeof buf - 1 - len);
        if (n < 0) { if (errno == EINTR) continue; break; }
        if (n == 0) break;
        len += n;
        char *start = buf;
        for (;;) {
            char *nl = memchr(start, '\n', len - (start - buf));
            if (!nl) break;
            *nl = 0;
            if (nl > start) command(start);
            start = nl + 1;
        }
        len -= start - buf;
        memmove(buf, start, len);
        if (len == sizeof buf - 1) len = 0;
    }
    libvlc_media_player_stop(mp);
    libvlc_media_player_release(mp);
    libvlc_release(vlc);
    return 0;
}
