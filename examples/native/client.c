#include "libarithmetic.h"
#include <assert.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>

static void *wrong_thread(void *context) {
    int64_t result = 19;
    assert(cog_export_add(context, 1, 2, &result) == COG_NATIVE_FAILURE);
    assert(cog_native_close(context) == COG_NATIVE_FAILURE);
    assert(result == 19);
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 2);
    cog_native_context *context = cog_native_open();
    assert(context != NULL);
    int64_t result = 0;
    assert(cog_export_add(context, 20, 22, &result) == COG_NATIVE_OK);
    assert(result == 42);
    assert(cog_export_divide(context, 20, 0, &result) == COG_NATIVE_ERROR);
    assert(result == 42);
    assert(cog_export_add(context, INT64_MAX, 1, &result) == COG_NATIVE_ERROR);
    assert(result == 42);
    assert(cog_export_add(context, 1, 2, NULL) == COG_NATIVE_FAILURE);
    pthread_t thread;
    assert(pthread_create(&thread, NULL, wrong_thread, context) == 0);
    assert(pthread_join(thread, NULL) == 0);
    void *other = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    assert(other != NULL);
    int32_t (*other_add)(cog_native_context *, int64_t, int64_t, int64_t *) =
        (int32_t (*)(cog_native_context *, int64_t, int64_t, int64_t *))dlsym(other, "cog_export_add");
    int32_t (*other_close)(cog_native_context *) =
        (int32_t (*)(cog_native_context *))dlsym(other, "cog_native_close");
    assert(other_add != NULL && other_close != NULL);
    assert(other_add(context, 1, 2, &result) == COG_NATIVE_FAILURE);
    assert(other_close(context) == COG_NATIVE_FAILURE);
    assert(result == 42);
    assert(dlclose(other) == 0);
    for (int64_t i = 0; i < 100000; ++i) {
        assert(cog_export_add(context, i, 1, &result) == COG_NATIVE_OK);
        assert(result == i + 1);
    }
    assert(cog_native_close(context) == COG_NATIVE_OK);
}
