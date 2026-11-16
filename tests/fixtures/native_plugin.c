#include "cog_plugin.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#include <windows.h>

static FILE *open_marker(const char *path) {
    int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, NULL, 0);
    if (length <= 0) return NULL;
    wchar_t *wide = calloc((size_t)length, sizeof(wchar_t));
    if (!wide) return NULL;
    FILE *file = NULL;
    if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, length) == length) {
        file = _wfopen(wide, L"wb");
    }
    free(wide);
    return file;
}
#else
static FILE *open_marker(const char *path) { return fopen(path, "wb"); }
#endif

#ifndef TEST_PLUGIN_VERSION
#define TEST_PLUGIN_VERSION COG_ABI_VERSION
#endif
#ifndef TEST_PLUGIN_SIZE
#define TEST_PLUGIN_SIZE sizeof(CogPluginV3)
#endif
#ifndef TEST_FUNCTION_SIZE
#define TEST_FUNCTION_SIZE sizeof(CogFunctionV3)
#endif

typedef struct { int64_t count; CogHandle saved; char *marker; } State;
/* The current host packs a 52-bit generation and 12-bit slot into CogHandle.
 * This ABI fixture treats tokens as opaque and deliberately does not decode
 * that implementation detail. */
static CogStatus create(void **output) {
    State *state = calloc(1, sizeof(State));
    if (!state) return COG_LIMIT;
    *output = state;
#ifdef TEST_CREATE_FAILURE
    const char *marker = TEST_CREATE_MARKER;
    state->marker = malloc(strlen(marker) + 1);
    if (!state->marker) return COG_LIMIT;
    memcpy(state->marker, marker, strlen(marker) + 1);
    return COG_FAILURE;
#else
    return COG_OK;
#endif
}
static void destroy(void *pointer) {
    State *state = (State *)pointer;
    if (!state) return;
    if (state->marker) {
        FILE *file = open_marker(state->marker);
        if (file) { fputs("destroyed", file); fclose(file); }
        free(state->marker);
    }
    free(state);
}
static CogStatus invoke(void *pointer, const CogHostV3 *host, void *context,
                        const CogHandle *arguments, size_t count, CogHandle *output) {
    State *state = (State *)pointer;
    CogValueView value = {0};
    if (host->size != sizeof(CogHostV3) || host->version != COG_ABI_VERSION || count != 1) return COG_INVALID;
    if (host->inspect(context, arguments[0], &value) != COG_OK) return COG_INVALID;
    if (value.kind == COG_INT) {
        if ((int64_t)value.integer == 0) { *output = 0; return COG_OK; }
        if ((int64_t)value.integer == -1) { *output = state->saved; return COG_OK; }
        if ((int64_t)value.integer == -4) {
            CogValueView invalid = {0};
            if (host->inspect(context, 0, &invalid) != COG_INVALID) return COG_FAILURE;
            *output = arguments[0];
            return COG_OK;
        }
        if ((int64_t)value.integer == -2) {
            /* Expired handles must be rejected by host operations as well. */
            if (host->inspect(context, state->saved, &value) != COG_INVALID) return COG_FAILURE;
        } else if ((int64_t)value.integer == -3) {
            value.kind = COG_STR;
            value.bytes.data = (const uint8_t *)"x";
            value.bytes.len = (size_t)-1;
            return host->make(context, &value, output);
        }
        state->saved = arguments[0];
        value.kind = COG_INT;
        value.integer = ++state->count;
        return host->make(context, &value, output);
    }
    if (value.kind == COG_STR) {
        free(state->marker);
        state->marker = (char *)malloc(value.bytes.len + 1);
        if (!state->marker) return COG_LIMIT;
        memcpy(state->marker, value.bytes.data, value.bytes.len);
        state->marker[value.bytes.len] = 0;
    }
    *output = arguments[0];
    return COG_OK;
}
static const uint8_t parameters[] = {COG_ANY};
static const CogFunctionV3 functions[] = {{
    TEST_FUNCTION_SIZE,
    {(const uint8_t *)"host::probe", 11}, {parameters, 1},
    COG_ANY, 0, COG_FS_WRITE, 0, invoke, NULL, NULL, NULL, NULL
}};
static const CogPluginV3 plugin = {
    TEST_PLUGIN_SIZE, TEST_PLUGIN_VERSION, 1, functions, create, destroy
};
COG_EXPORT const CogPluginV3 *cog_plugin_v3(void) { return &plugin; }
