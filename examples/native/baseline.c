#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int64_t numeric(void) {
    int64_t sum = 0;
    for (int64_t i = 0; i < 5000000; ++i)
        sum += (i * 3) % 97;
    return sum;
}

static int64_t pipeline(void) {
    int64_t values[] = {2, 3, 5, 7};
    int64_t sum = 0;
    for (int64_t i = 0; i < 1000000; ++i) {
        int64_t index = i % 4;
        int64_t value = ((values[index] + i) * 7 + 3) % 997;
        values[index] = value;
        sum += value;
    }
    return sum;
}

int main(int argc, char **argv) {
    if (argc != 2) return 1;
    printf("%lld\n", (long long)(strcmp(argv[1], "numeric") == 0 ? numeric() : pipeline()));
}
