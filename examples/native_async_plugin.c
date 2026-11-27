#define _POSIX_C_SOURCE 200809L
#include "cog_plugin.h"
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <time.h>

typedef struct {
    pthread_t thread;
    int64_t delay_ms;
    _Atomic int64_t result;
    _Atomic int done;
    _Atomic int cancelled;
    int joined;
} Operation;

typedef struct {
    Operation *operation;
    const CogHostV3 *host;
    void *wake_context;
} WorkerArgs;

static void *worker(void *pointer) {
    WorkerArgs *args = pointer;
    Operation *operation = args->operation;
    const CogHostV3 *host = args->host;
    void *wake_context = args->wake_context;
    free(args);
    struct timespec pause = { .tv_sec = 0, .tv_nsec = 1000000 };
    for (int64_t elapsed = 0; elapsed < operation->delay_ms; elapsed++) {
        if (atomic_load_explicit(&operation->cancelled, memory_order_acquire)) return NULL;
        nanosleep(&pause, NULL);
    }
    if (atomic_load_explicit(&operation->cancelled, memory_order_acquire)) return NULL;
    atomic_store_explicit(&operation->result, operation->delay_ms * 2, memory_order_release);
    atomic_store_explicit(&operation->done, 1, memory_order_release);
    /* Async worker threads may only call this thread-safe wake function. A
     * full wake queue must be retried, but cancellation still stops promptly. */
    while (host->wake(wake_context) == COG_LIMIT) {
        if (atomic_load_explicit(&operation->cancelled, memory_order_acquire)) break;
        nanosleep(&pause, NULL);
    }
    return NULL;
}

static CogStatus start(void *plugin, const CogHostV3 *host, void *frame,
                       void *wake_context, const CogHandle *arguments,
                       size_t count, void **output) {
    (void)plugin;
    if (!host || !frame || !wake_context || !arguments || !output || count != 1) return COG_INVALID;
    CogValueView input = {0};
    CogStatus status = host->inspect(frame, arguments[0], &input);
    if (status != COG_OK) return status;
    if (input.kind != COG_INT || (int64_t)input.integer < 1 || (int64_t)input.integer > 5000) {
        static const uint8_t message[] = "delay must be an int between 1 and 5000 ms";
        (void)host->set_error(frame, (CogBytes){ message, sizeof(message) - 1 });
        return COG_INVALID;
    }
    static const uint8_t started[] = "start\n";
    status = host->write_output(frame, (CogBytes){ started, sizeof(started) - 1 });
    if (status != COG_OK) return status;

    Operation *operation = calloc(1, sizeof(*operation));
    WorkerArgs *args = calloc(1, sizeof(*args));
    if (!operation || !args) {
        free(operation);
        free(args);
        return COG_LIMIT;
    }
    operation->delay_ms = (int64_t)input.integer;
    atomic_init(&operation->result, 0);
    atomic_init(&operation->done, 0);
    atomic_init(&operation->cancelled, 0);
    args->operation = operation;
    args->host = host;
    args->wake_context = wake_context;
    if (pthread_create(&operation->thread, NULL, worker, args) != 0) {
        free(args);
        free(operation);
        return COG_FAILURE;
    }
    *output = operation;
    return COG_OK;
}

static CogStatus poll(void *plugin, void *pointer, const CogHostV3 *host,
                      void *frame, CogHandle *output) {
    (void)plugin;
    if (!pointer || !host || !frame || !output) return COG_INVALID;
    Operation *operation = pointer;
    if (!atomic_load_explicit(&operation->done, memory_order_acquire)) return COG_PENDING;
    CogValueView value = { .kind = COG_INT,
                           .integer = (uint64_t)atomic_load_explicit(&operation->result, memory_order_acquire) };
    CogStatus status = host->make(frame, &value, output);
    if (status != COG_OK) return status;
    static const uint8_t finished[] = "finish\n";
    return host->write_output(frame, (CogBytes){ finished, sizeof(finished) - 1 });
}

static void cancel(void *plugin, void *pointer) {
    (void)plugin;
    if (!pointer) return;
    Operation *operation = pointer;
    atomic_store_explicit(&operation->cancelled, 1, memory_order_release);
    (void)pthread_join(operation->thread, NULL);
    operation->joined = 1;
}

static void destroy_operation(void *plugin, void *pointer) {
    (void)plugin;
    if (!pointer) return;
    Operation *operation = pointer;
    if (!operation->joined) (void)pthread_join(operation->thread, NULL);
    free(operation);
}

static const uint8_t parameters[] = { COG_INT };
static const CogFunctionV3 functions[] = {{
    .size = sizeof(CogFunctionV3),
    .name = { (const uint8_t *)"host::delayed", 13 },
    .parameters = { parameters, 1 },
    .result = COG_INT,
    .asynchronous = 1,
    .effects = COG_OUTPUT,
    .mutations = 0,
    .invoke = NULL,
    .start = start,
    .poll = poll,
    .cancel = cancel,
    .destroy_operation = destroy_operation,
}};

static const CogPluginV3 plugin = {
    .size = sizeof(CogPluginV3),
    .version = COG_ABI_VERSION,
    .function_count = 1,
    .functions = functions,
    .create = NULL,
    .destroy = NULL,
};

COG_EXPORT const CogPluginV3 *cog_plugin_v3(void) { return &plugin; }
