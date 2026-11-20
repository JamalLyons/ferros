// Code of the `long_jump` fixture, compiled and linked by `build.rs` (arm64 macOS). It
// is C because `setjmp` returns twice, which Rust cannot call soundly.
//
// Without arguments, `main` (marker `call_middle`) calls `middle`, which calls `jumper`
// (marker `call_jumper`); `jumper` (marker `in_jumper`) `longjmp`s back to the `setjmp` in
// `main`, past both callers. `after_jump` is the first line after `setjmp` returns again.
//
// With `--handler`, `roundtrip` raises SIGUSR1 at itself; the handler `on_sigusr1` (marker
// `in_handler`) leaves by `siglongjmp` to the `sigsetjmp` in `roundtrip`, which then calls
// `raise(0)` (marker `raise_again`). Both `raise` calls come from `roundtrip`'s frame, so
// the second reaches the instruction where the signal interrupted the first with the same
// stack pointer, without a signal.

#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

static jmp_buf outer;
static sigjmp_buf handler_exit;
static volatile sig_atomic_t handled;

__attribute__((noinline)) static void jumper(int value)
{
    if (value > 0) {
        longjmp(outer, value); // BREAK: in_jumper
    }
}

__attribute__((noinline)) static int middle(int value)
{
    jumper(value); // BREAK: call_jumper
    return value;
}

static void on_sigusr1(int signal)
{
    handled = signal; // BREAK: in_handler
    siglongjmp(handler_exit, 1);
}

__attribute__((noinline)) static void roundtrip(void)
{
    if (sigsetjmp(handler_exit, 1) == 0) {
        raise(SIGUSR1);
    }
    raise(0); // BREAK: raise_again
    printf("handler left by siglongjmp: %s\n", handled == SIGUSR1 ? "true" : "false");
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "--handler") == 0) {
        signal(SIGUSR1, on_sigusr1);
        roundtrip();
        return 0;
    }
    volatile int jumped = setjmp(outer);
    if (jumped == 0) {
        middle(5); // BREAK: call_middle
    }
    int after = jumped; // BREAK: after_jump
    printf("long jump: %d\n", after);
    return 0;
}
