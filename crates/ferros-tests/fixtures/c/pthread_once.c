// Code of the `pthread_once` fixture, compiled and linked by `build.rs` (arm64 macOS).
//
// `main` (marker `call_once`) calls `pthread_once` on a static `pthread_once_t`. The
// callback `init_routine` (marker `in_init`) is entered from `__pthread_once_handler`, a
// static function of `libsystem_pthread` in the dyld shared cache.

#include <pthread.h>
#include <stdio.h>

static pthread_once_t once = PTHREAD_ONCE_INIT;
static volatile int initialized;

void init_routine(void)
{
    initialized = 1; // BREAK: in_init
}

int main(void)
{
    pthread_once(&once, init_routine); // BREAK: call_once
    printf("pthread once: %d\n", initialized);
    return 0;
}
