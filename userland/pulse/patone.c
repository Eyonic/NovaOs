/* patone: play a sine tone through PulseAudio with libpulse-simple, as a Linux program
 * that uses the sound server would (Nova's built-in server, kernel/src/pulse.rs).
 *
 *   patone [hz] [ms] [s16|float] [rate]     default: 1000 Hz, 1000 ms, s16, 48000 Hz
 */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <pulse/error.h>
#include <pulse/simple.h>

int main(int argc, char **argv) {
    int hz = argc > 1 ? atoi(argv[1]) : 1000;
    int ms = argc > 2 ? atoi(argv[2]) : 1000;
    int as_float = argc > 3 && strcmp(argv[3], "float") == 0;
    int rate = argc > 4 ? atoi(argv[4]) : 48000;
    pa_sample_spec ss = { .format = as_float ? PA_SAMPLE_FLOAT32LE : PA_SAMPLE_S16LE, .rate = rate, .channels = 2 };
    int err = 0;
    pa_simple *s = pa_simple_new(NULL, "patone", PA_STREAM_PLAYBACK, NULL, "tone", &ss, NULL, NULL, &err);
    if (!s) {
        fprintf(stderr, "patone: connecting failed: %s\n", pa_strerror(err));
        return 1;
    }
    long frames = (long)rate * ms / 1000, fade = rate / 200;
    enum { CHUNK = 1024 };
    float fbuf[CHUNK * 2];
    short sbuf[CHUNK * 2];
    for (long f = 0; f < frames; f += CHUNK) {
        int n = frames - f < CHUNK ? (int)(frames - f) : CHUNK;
        for (int i = 0; i < n; i++) {
            long k = f + i, edge = k < frames - k ? k : frames - k;
            double v = 0.5 * sin(2 * M_PI * hz * (double)k / rate) * (edge < fade ? (double)edge / fade : 1.0);
            fbuf[2 * i] = fbuf[2 * i + 1] = (float)v;
            sbuf[2 * i] = sbuf[2 * i + 1] = (short)(v * 32767);
        }
        const void *data = as_float ? (const void *)fbuf : (const void *)sbuf;
        if (pa_simple_write(s, data, (size_t)n * pa_frame_size(&ss), &err) < 0) {
            fprintf(stderr, "patone: write failed: %s\n", pa_strerror(err));
            return 1;
        }
    }
    pa_usec_t latency = pa_simple_get_latency(s, &err);
    if (pa_simple_drain(s, &err) < 0) {
        fprintf(stderr, "patone: drain failed: %s\n", pa_strerror(err));
        return 1;
    }
    pa_simple_free(s);
    printf("patone: played %d Hz for %d ms (%s, %d Hz), latency %llu ms\n", hz, ms, as_float ? "float32le" : "s16le", rate,
           (unsigned long long)latency / 1000);
    return 0;
}
