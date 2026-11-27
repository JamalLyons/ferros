// Built at -O2 so the caller's argument stays in a volatile register at the call.
static volatile unsigned fixture_number = 37;

__attribute__((noinline)) static unsigned values_register_stop(unsigned input) {
    __asm__ volatile("" : "+r"(input));
    return input + 1;
}

__attribute__((noinline)) static unsigned values_register_caller(unsigned volatile_arg) {
    unsigned result = values_register_stop(volatile_arg);
    // Keep the caller on the stack and stop the optimizer from making a tail call.
    __asm__ volatile("" : "+r"(result));
    return result;
}

__attribute__((constructor)) static void values_register_init(void) {
    fixture_number = values_register_caller(fixture_number);
}
