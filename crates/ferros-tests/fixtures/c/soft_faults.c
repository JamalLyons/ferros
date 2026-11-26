// Code of the `soft_faults` fixture, compiled and linked by `build.rs` (arm64 macOS). It
// is C because each mode needs code a Rust fixture cannot write without `unsafe`.
//
// With `--null-store`, `main` stores through a null pointer (marker `null_store`): a real
// `EXC_BAD_ACCESS` that faults again at the same pc each time it is retried.
//
// With `--handler-once`, `main` installs a counting SIGUSR1 handler (marker `in_handler`),
// raises SIGUSR1 once, and prints `handler runs: <count>`.
//
// With `--software-exception`, `main` raises an `EXC_SOFTWARE` exception that is not a
// Unix signal (code `EXC_UNIX_ABORT`) on its own thread, by sending a
// `mach_exception_raise` request to the task's `EXC_SOFTWARE` handler (the debugger's
// exception port), and prints `exception reply: <kern_return_t>` once it is answered.
//
// With `--guard-page`, `main` maps two pages, fills the first with 0xa5, makes the
// second `PROT_NONE`, prints `boundary: 0x<address of the second page>` and sleeps
// forever, so a debugger can read across a readable-to-protected page boundary.
// `--guard-page-with-tail` adds a readable third page filled with 0x5a.
// `--read-only-page` maps a page filled with 0xa5 twice: as an ordinary writable page and
// as a permanent view whose maximum protection is read-only, and prints
// `read-only page: 0x<view> backing: 0x<ordinary page>`, so a debugger write to the view
// is refused and its effect on the shared bytes can be read through the ordinary page.

#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile sig_atomic_t handler_runs;

static void on_sigusr1(int signal)
{
    handler_runs += signal == SIGUSR1; // BREAK: in_handler
}

// `mach_exception_raise` (MIG id 2405) request and reply, packed to 4 bytes as MIG does.
#pragma pack(push, 4)
typedef struct
{
    mach_msg_header_t head;
    mach_msg_body_t body;
    mach_msg_port_descriptor_t thread;
    mach_msg_port_descriptor_t task;
    NDR_record_t ndr;
    exception_type_t exception;
    mach_msg_type_number_t code_count;
    int64_t code[2];
} raise_request;

typedef struct
{
    mach_msg_header_t head;
    NDR_record_t ndr;
    kern_return_t ret_code;
    mach_msg_trailer_t trailer;
} raise_reply;
#pragma pack(pop)

// `EXC_UNIX_ABORT` from <mach/exception_types.h>.
#define SOFTWARE_CODE 0x10002

static int raise_software_exception(void)
{
    exception_mask_t masks[EXC_TYPES_COUNT];
    mach_port_t handlers[EXC_TYPES_COUNT];
    exception_behavior_t behaviors[EXC_TYPES_COUNT];
    thread_state_flavor_t flavors[EXC_TYPES_COUNT];
    mach_msg_type_number_t count = EXC_TYPES_COUNT;
    kern_return_t kr = task_get_exception_ports(
        mach_task_self(), EXC_MASK_SOFTWARE, masks, &count, handlers, behaviors, flavors);
    if (kr != KERN_SUCCESS || count == 0 || !MACH_PORT_VALID(handlers[0])) {
        printf("no EXC_SOFTWARE handler: %d\n", kr);
        return 1;
    }
    mach_port_t reply_port = mig_get_reply_port();
    union
    {
        raise_request request;
        raise_reply reply;
    } message;
    memset(&message, 0, sizeof message);
    message.request.head.msgh_bits =
        MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE) | MACH_MSGH_BITS_COMPLEX;
    message.request.head.msgh_size = sizeof message.request;
    message.request.head.msgh_remote_port = handlers[0];
    message.request.head.msgh_local_port = reply_port;
    message.request.head.msgh_id = 2405;
    message.request.body.msgh_descriptor_count = 2;
    message.request.thread.name = mach_thread_self();
    message.request.thread.disposition = MACH_MSG_TYPE_COPY_SEND;
    message.request.thread.type = MACH_MSG_PORT_DESCRIPTOR;
    message.request.task.name = mach_task_self();
    message.request.task.disposition = MACH_MSG_TYPE_COPY_SEND;
    message.request.task.type = MACH_MSG_PORT_DESCRIPTOR;
    message.request.ndr = NDR_record;
    message.request.exception = EXC_SOFTWARE;
    message.request.code_count = 2;
    message.request.code[0] = SOFTWARE_CODE;
    message.request.code[1] = 0;
    kr = mach_msg(&message.request.head, MACH_SEND_MSG | MACH_RCV_MSG, sizeof message.request, sizeof message,
                  reply_port, MACH_MSG_TIMEOUT_NONE, MACH_PORT_NULL);
    mach_port_deallocate(mach_task_self(), message.request.thread.name);
    if (kr != MACH_MSG_SUCCESS) {
        printf("exception send failed: %d\n", kr);
        return 1;
    }
    printf("exception reply: %d\n", message.reply.ret_code); // BREAK: after_exception
    return 0;
}

static int hold_read_only_page(void)
{
    mach_port_t task = mach_task_self();
    mach_vm_address_t backing = 0;
    kern_return_t kr = mach_vm_allocate(task, &backing, vm_page_size, VM_FLAGS_ANYWHERE);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "allocate backing page: %d\n", kr);
        return 1;
    }
    memset((void *)backing, 0xa5, vm_page_size);
    memory_object_size_t size = vm_page_size;
    mach_port_t entry = MACH_PORT_NULL;
    kr = mach_make_memory_entry_64(task, &size, backing, VM_PROT_READ | VM_PROT_WRITE, &entry, MACH_PORT_NULL);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "make memory entry: %d\n", kr);
        return 1;
    }
    // The view shares the backing page's memory, is permanent, and has a read-only
    // maximum protection, so VM_PROT_COPY may not add write access to it. A refusal can
    // strip the view's access (macOS 15); the backing page still shows its bytes.
    mach_vm_address_t view = 0;
    kr = mach_vm_map(task, &view, size, 0, VM_FLAGS_ANYWHERE | VM_FLAGS_PERMANENT, entry, 0, FALSE, VM_PROT_READ,
                     VM_PROT_READ, VM_INHERIT_DEFAULT);
    if (kr != KERN_SUCCESS) {
        fprintf(stderr, "map permanent read-only view: %d\n", kr);
        return 1;
    }
    printf("read-only page: 0x%llx backing: 0x%llx\n", view, backing);
    fflush(stdout);
    for (;;) {
        sleep(1);
    }
}

int main(int argc, char **argv)
{
    if (argc > 1 && strcmp(argv[1], "--null-store") == 0) {
        volatile int *null = 0;
        *null = 1; // BREAK: null_store
        return 0;
    }
    if (argc > 1 && strcmp(argv[1], "--handler-once") == 0) {
        signal(SIGUSR1, on_sigusr1);
        raise(SIGUSR1);
        printf("handler runs: %d\n", (int)handler_runs);
        return 0;
    }
    if (argc > 1 && strcmp(argv[1], "--software-exception") == 0) {
        return raise_software_exception();
    }
    if (argc > 1 && strcmp(argv[1], "--read-only-page") == 0) {
        return hold_read_only_page();
    }
    int readable_tail = argc > 1 && strcmp(argv[1], "--guard-page-with-tail") == 0;
    if (readable_tail || (argc > 1 && strcmp(argv[1], "--guard-page") == 0)) {
        long page = sysconf(_SC_PAGESIZE);
        unsigned char *pages = mmap(NULL, (readable_tail ? 3 : 2) * page,
                                    PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
        if (pages == MAP_FAILED || mprotect(pages + page, page, PROT_NONE) != 0) {
            perror("guard page");
            return 1;
        }
        memset(pages, 0xa5, page);
        if (readable_tail) {
            memset(pages + 2 * page, 0x5a, page);
        }
        printf("boundary: 0x%lx\n", (unsigned long)(pages + page));
        fflush(stdout);
        for (;;) {
            sleep(1);
        }
    }
    printf("usage: --null-store | --handler-once | --software-exception | --guard-page | --guard-page-with-tail | --read-only-page\n");
    return 2;
}
