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

#include <mach/mach.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

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
    printf("usage: --null-store | --handler-once | --software-exception\n");
    return 2;
}
