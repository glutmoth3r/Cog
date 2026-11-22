#define _POSIX_C_SOURCE 200809L
#include "libarithmetic.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

static uint64_t now_ns(void) {
    struct timespec time;
    assert(clock_gettime(CLOCK_MONOTONIC, &time) == 0);
    return (uint64_t)time.tv_sec * UINT64_C(1000000000) + (uint64_t)time.tv_nsec;
}

int main(void) {
    const int64_t iterations = 100000;
    uint64_t started = now_ns();
    cog_native_context *context = cog_native_open();
    uint64_t init = now_ns() - started;
    assert(context != NULL);
    int64_t sum = 0;
    started = now_ns();
    for (int64_t i = 0; i < iterations; ++i) {
        int64_t result;
        assert(cog_export_add(context, i, 1, &result) == COG_NATIVE_OK);
        sum += result;
    }
    uint64_t elapsed = now_ns() - started;
    assert(sum == iterations * (iterations + 1) / 2);
    assert(cog_native_close(context) == COG_NATIVE_OK);
    printf("%llu %.3f\n", (unsigned long long)init, (double)elapsed / (double)iterations);
}
