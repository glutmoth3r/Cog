/* Fixture for the ABI v3 ownership and error paths. Every async operation
 * records its callbacks in the plugin state; `probe::events` drains that log. */
#include "cog_plugin.h"
#include <stdlib.h>
#include <string.h>

typedef struct { char log[256]; size_t len; } State;
typedef enum { FOREVER, READY, START_ERROR, PENDING_ERROR, BAD_RESULT } Mode;
typedef struct { Mode mode; } Operation;

static CogBytes text(const char *value) {
    CogBytes bytes = {(const uint8_t *)value, strlen(value)};
    return bytes;
}
static void record(State *state, const char *event) {
    size_t length = strlen(event);
    if (length > sizeof(state->log) - state->len) return;
    memcpy(state->log + state->len, event, length);
    state->len += length;
}
static CogStatus create(void **output) {
    *output = calloc(1, sizeof(State));
    return *output ? COG_OK : COG_LIMIT;
}
static void destroy(void *state) { free(state); }

static CogStatus begin(Mode mode, void *state, const CogHostV3 *host, void *frame,
                       void **output) {
    Operation *operation = calloc(1, sizeof(Operation));
    if (!operation) return COG_LIMIT;
    operation->mode = mode;
    *output = operation;
    record((State *)state, "start;");
    if (mode == START_ERROR) (void)host->set_error(frame, text("start reported an error"));
    return COG_OK;
}
#define START(name, mode)                                                          \
    static CogStatus name(void *state, const CogHostV3 *host, void *frame,         \
                          void *wake, const CogHandle *arguments, size_t count,    \
                          void **output) {                                         \
        (void)wake; (void)arguments; (void)count;                                  \
        return begin(mode, state, host, frame, output);                            \
    }
START(start_forever, FOREVER)
START(start_ready, READY)
START(start_error, START_ERROR)
START(start_pending_error, PENDING_ERROR)
START(start_bad_result, BAD_RESULT)

static CogStatus poll_operation(void *state, void *pointer, const CogHostV3 *host,
                                void *frame, CogHandle *output) {
    Operation *operation = (Operation *)pointer;
    CogValueView value = {0};
    (void)state;
    switch (operation->mode) {
    case PENDING_ERROR:
        (void)host->set_error(frame, text("poll reported an error"));
        return COG_PENDING;
    case READY:
        value.kind = COG_INT;
        value.integer = 7;
        return host->make(frame, &value, output);
    case BAD_RESULT:
        value.kind = COG_STR;
        value.bytes = text("not an int");
        return host->make(frame, &value, output);
    default:
        return COG_PENDING;
    }
}
static void cancel(void *state, void *operation) {
    (void)operation;
    record((State *)state, "cancel;");
}
static void destroy_operation(void *state, void *operation) {
    record((State *)state, "destroy;");
    free(operation);
}

static CogStatus events(void *pointer, const CogHostV3 *host, void *frame,
                        const CogHandle *arguments, size_t count, CogHandle *output) {
    State *state = (State *)pointer;
    CogValueView value = {0};
    CogStatus status;
    (void)arguments; (void)count;
    value.kind = COG_STR;
    value.bytes.data = (const uint8_t *)state->log;
    value.bytes.len = state->len;
    status = host->make(frame, &value, output);
    state->len = 0;
    return status;
}
static CogStatus oversized_output(void *state, const CogHostV3 *host, void *frame,
                                  const CogHandle *arguments, size_t count,
                                  CogHandle *output) {
    CogBytes bytes = {(const uint8_t *)"x", (size_t)-1};
    CogValueView value = {0};
    (void)state; (void)arguments; (void)count;
    value.kind = COG_INT;
    value.integer = host->write_output(frame, bytes);
    return host->make(frame, &value, output);
}

#define NAME(value) {(const uint8_t *)value, sizeof(value) - 1}
#define ASYNC(name, result, start)                                                 \
    {sizeof(CogFunctionV3), NAME(name), {NULL, 0}, result, 1, 0, 0, NULL, start,   \
     poll_operation, cancel, destroy_operation}
#define SYNC(name, result, invoke)                                                 \
    {sizeof(CogFunctionV3), NAME(name), {NULL, 0}, result, 0, 0, 0, invoke, NULL,  \
     NULL, NULL, NULL}
static const CogFunctionV3 functions[] = {
    ASYNC("probe::forever", COG_INT, start_forever),
    ASYNC("probe::ready", COG_INT, start_ready),
    ASYNC("probe::start_error", COG_INT, start_error),
    ASYNC("probe::pending_error", COG_INT, start_pending_error),
    ASYNC("probe::bad_result", COG_INT, start_bad_result),
    SYNC("probe::events", COG_STR, events),
    SYNC("probe::oversized_output", COG_INT, oversized_output),
};
static const CogPluginV3 plugin = {
    sizeof(CogPluginV3), COG_ABI_VERSION,
    sizeof(functions) / sizeof(functions[0]), functions, create, destroy
};
COG_EXPORT const CogPluginV3 *cog_plugin_v3(void) { return &plugin; }
