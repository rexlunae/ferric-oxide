/* MIT: verify firmware wall time independently of the guest's current date. */
#define _POSIX_C_SOURCE 200809L
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static void fail(const char *reason) {
    fprintf(stderr, "FERRIC_TEST_FAIL clock: %s (errno=%d)\n", reason, errno);
    exit(1);
}

static int64_t seconds(const char *arg) {
    char *end;
    errno = 0;
    long long value = strtoll(arg, &end, 10);
    if (errno || end == arg || *end || value < 0 || value > INT64_MAX / 1000000000)
        fail("invalid epoch bound");
    return value;
}

static int64_t read_clock(clockid_t id) {
    struct timespec ts;
    if (clock_gettime(id, &ts) || ts.tv_sec < 0 ||
        ts.tv_nsec < 0 || ts.tv_nsec >= 1000000000 ||
        ts.tv_sec > (INT64_MAX - ts.tv_nsec) / 1000000000)
        fail("invalid clock_gettime result");
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

int main(int argc, char **argv) {
    int offset_mode = argc == 4 && !strcmp(argv[1], "--offset");
    if (argc != 3 && !offset_mode)
        fail("usage: ferric-clock-test [--offset] MIN_SECONDS MAX_SECONDS");
    int64_t min = seconds(argv[1 + offset_mode]), max = seconds(argv[2 + offset_mode]);
    if (max < min)
        fail("reversed epoch bounds");

    int64_t real_before = read_clock(CLOCK_REALTIME);
    int64_t mono_before = read_clock(CLOCK_MONOTONIC);
    printf("clock sample: realtime=%lld monotonic=%lld\n",
           (long long)real_before, (long long)mono_before);
    if (mono_before >= real_before)
        fail("wall-clock initialization changed the monotonic clock");
    int64_t sample = offset_mode ? real_before - mono_before : real_before;
    if (sample / 1000000000 < min || sample / 1000000000 > max)
        fail("clock outside externally supplied RTC range");

    struct timespec delay = { .tv_sec = 0, .tv_nsec = 200000000 };
    while (nanosleep(&delay, &delay)) {
        if (errno != EINTR)
            fail("nanosleep failed");
    }
    int64_t mono_after = read_clock(CLOCK_MONOTONIC);
    int64_t real_after = read_clock(CLOCK_REALTIME);
    if (mono_after - mono_before < 200000000 || real_after <= real_before)
        fail("clocks did not advance");
    /* Bracketed reads allow scheduling between the two clock reads. */
    int64_t drift = (real_after - real_before) - (mono_after - mono_before);
    if (drift < 0 || drift > 100000000)
        fail("realtime and monotonic elapsed times disagree");
    puts("FERRIC_TEST_PASS clock");
    return 0;
}
