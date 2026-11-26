// Keep every handle open until the debugger inspects dyld's image list.
#include <dlfcn.h>
#include <stdio.h>
#include <unistd.h>

// Breakpoint reached only after every path resolved to the same symbol.
void aliases_loaded(void)
{
}

int main(int argc, char **argv)
{
    // The harness appends its cleanup tag after the five fixture arguments.
    if (argc != 7)
        return 1;
    if (chdir(argv[1]) != 0) {
        perror("chdir");
        return 1;
    }

    void *first_symbol = NULL;
    for (int i = 2; i <= 5; i++) {
        void *handle = dlopen(argv[i], RTLD_NOW | RTLD_LOCAL | RTLD_FIRST);
        if (handle == NULL) {
            fprintf(stderr, "dlopen %s: %s\n", argv[i], dlerror());
            return 1;
        }
        void *symbol = dlsym(handle, "ferros_alias_fixture_symbol");
        if (symbol == NULL) {
            fprintf(stderr, "dlsym %s: %s\n", argv[i], dlerror());
            return 1;
        }
        if (i == 2)
            first_symbol = symbol;
        else if (symbol != first_symbol) {
            fprintf(stderr, "%s mapped the fixture library a second time\n", argv[i]);
            return 1;
        }
    }

    aliases_loaded();
    return 0;
}
