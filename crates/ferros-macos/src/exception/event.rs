//! Mapping a Mach exception to a portable stop event.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Get the thread state flavor for the given architecture.
///
/// Returns the appropriate thread state flavor constant for use with
/// `thread_get_state()` and `thread_set_state()`.
///
/// ## Thread State Flavors
///
/// - **ARM64**: Returns `6` (`ARM_THREAD_STATE64`)
/// - **x86-64**: Returns `4` (`x86_THREAD_STATE64`)
///
/// See: [thread_get_state(3) man page](https://developer.apple.com/documentation/kernel/1418576-thread_get_state/)
pub(crate) fn thread_state_flavor_for_arch(architecture: Architecture) -> c_int
{
    match architecture {
        Architecture::Arm64 => 6,
        Architecture::X86_64 => 4,
        Architecture::Unknown(_) => 6,
    }
}

/// Convert a Mach exception to a [`StopEvent`].
///
/// Maps Mach exception types to portable stop events.
///
/// ## Exception Types
///
/// - `EXC_BREAKPOINT` with codes `[EXC_ARM_BREAKPOINT, 0]` -> `StopEvent::SingleStep`:
///   XNU's `handle_sw_step_debug` (`osfmk/arm64/sleh.c`) raises exactly these codes when
///   a hardware single-step completes. (On x86-64, `EXC_I386_SGL` is also 1.)
/// - any other `EXC_BREAKPOINT` -> `StopEvent::Breakpoint` with the raw `pc` (not rewound;
///   on arm64 it already points at the `brk`, whose codes are `[EXC_ARM_BREAKPOINT, pc]`,
///   and the engine maps it with `Arch::pc_after_trap`)
/// - `EXC_BAD_ACCESS` -> `StopEvent::Fault(MemoryAccess)`
/// - `EXC_BAD_INSTRUCTION` -> `StopEvent::Fault(IllegalInstruction)`
/// - `EXC_ARITHMETIC` -> `StopEvent::Fault(Arithmetic)`
///
///   These are synchronous faults, not held Unix signals: XNU keeps no soft-signal
///   decision for them, so a resume has no signal to deliver and the thread retries
///   the faulting instruction.
/// - `EXC_SOFTWARE/EXC_SOFT_SIGNAL` -> `StopEvent::Signal`, the held Unix signal
/// - any other `EXC_SOFTWARE` -> `StopEvent::Other`, which holds no signal. XNU raises
///   `EXC_SOFTWARE` itself only with `EXC_SOFT_SIGNAL`, but any holder of a send right
///   to the task's exception port can raise one with other codes (the
///   `EXC_UNIX_*` codes of `<mach/exception_types.h>`, or its own), and nothing is
///   held for such a stop: reporting it as a signal made a forwarding resume fail.
/// - anything else (or a breakpoint whose pc cannot be read) -> `StopEvent::Other`
///
/// See: [Mach Exception Types](https://developer.apple.com/documentation/kernel/1402149-exception_types/)
pub(crate) fn stop_event_from_exception(
    exception: exception_type_t,
    thread: ThreadId,
    pc: Option<u64>,
    codes: [i64; 2],
) -> StopEvent
{
    let signal = |signal| StopEvent::Signal { thread, signal };
    let fault = |kind, name: &str| StopEvent::Fault {
        thread,
        kind,
        detail: format!("{name}, codes {codes:?}"),
    };
    match exception as u32 {
        EXC_BREAKPOINT if codes == [EXC_ARM_BREAKPOINT, 0] => StopEvent::SingleStep { thread },
        EXC_BREAKPOINT => match pc {
            Some(pc) => StopEvent::Breakpoint {
                thread,
                pc: Address::from(pc),
            },
            None => StopEvent::Other {
                thread: Some(thread),
                reason: format!("EXC_BREAKPOINT (pc unreadable), codes {codes:?}"),
            },
        },
        EXC_BAD_ACCESS => fault(FaultKind::MemoryAccess, "EXC_BAD_ACCESS"),
        EXC_BAD_INSTRUCTION => fault(FaultKind::IllegalInstruction, "EXC_BAD_INSTRUCTION"),
        EXC_ARITHMETIC => fault(FaultKind::Arithmetic, "EXC_ARITHMETIC"),
        EXC_SOFTWARE if codes[0] == EXC_SOFT_SIGNAL => signal(codes[1] as c_int),
        // Not a held Unix signal: XNU keeps a soft-signal decision only for
        // `EXC_SOFT_SIGNAL`, so a `Signal` here would be one a forwarding resume is
        // refused for (`check_resume_request`).
        EXC_SOFTWARE => StopEvent::Other {
            thread: Some(thread),
            reason: format!("EXC_SOFTWARE, codes {codes:?}"),
        },
        other => StopEvent::Other {
            thread: Some(thread),
            reason: format!("Mach exception {other}, codes {codes:?}"),
        },
    }
}

/// Unix signal exception code from `<mach/exception_types.h>`.
pub(crate) const EXC_SOFT_SIGNAL: i64 = 0x10003;

/// `EXC_ARM_BREAKPOINT` from `<mach/arm/exception.h>`: the code of every arm64
/// `EXC_BREAKPOINT` (a `brk`, a hardware breakpoint, or a completed single-step).
pub(crate) const EXC_ARM_BREAKPOINT: i64 = 1;

#[cfg(test)]
mod tests
{
    #[allow(clippy::wildcard_imports)]
    use super::*;

    const THREAD: u64 = 0x1403;

    fn event(exception: u32, pc: Option<u64>, codes: [i64; 2]) -> StopEvent
    {
        stop_event_from_exception(exception as exception_type_t, ThreadId::from(THREAD), pc, codes)
    }

    #[test]
    fn completed_single_step_is_a_single_step_stop()
    {
        // What `handle_sw_step_debug` raises; the pc is irrelevant.
        assert_eq!(
            event(EXC_BREAKPOINT, Some(0x1_0000_0aac), [EXC_ARM_BREAKPOINT, 0]),
            StopEvent::SingleStep {
                thread: ThreadId::from(THREAD)
            }
        );
        assert_eq!(
            event(EXC_BREAKPOINT, None, [EXC_ARM_BREAKPOINT, 0]),
            StopEvent::SingleStep {
                thread: ThreadId::from(THREAD)
            }
        );
    }

    #[test]
    fn brk_is_a_breakpoint_stop_at_the_raw_pc()
    {
        // A `brk` carries its own address as the second code.
        assert_eq!(
            event(EXC_BREAKPOINT, Some(0x1_0000_0aa8), [EXC_ARM_BREAKPOINT, 0x1_0000_0aa8]),
            StopEvent::Breakpoint {
                thread: ThreadId::from(THREAD),
                pc: Address::from(0x1_0000_0aa8),
            }
        );
        // Any other breakpoint code is a breakpoint too, never a step.
        assert!(matches!(
            event(EXC_BREAKPOINT, Some(0x40), [2, 0]),
            StopEvent::Breakpoint { .. }
        ));
        assert!(matches!(
            event(EXC_BREAKPOINT, None, [EXC_ARM_BREAKPOINT, 0x40]),
            StopEvent::Other { .. }
        ));
    }

    #[test]
    fn other_exceptions_keep_their_mapping()
    {
        let thread = ThreadId::from(THREAD);
        for (exception, kind) in [
            (EXC_BAD_ACCESS, FaultKind::MemoryAccess),
            (EXC_BAD_INSTRUCTION, FaultKind::IllegalInstruction),
            (EXC_ARITHMETIC, FaultKind::Arithmetic),
        ] {
            let stop = event(exception, None, [1, 0]);
            assert!(
                matches!(stop, StopEvent::Fault { thread: t, kind: k, .. } if t == thread && k == kind),
                "exception {exception}: {stop:?}"
            );
            assert_eq!(stop.deliverable_signal(), None);
        }
        assert_eq!(
            event(EXC_SOFTWARE, None, [EXC_SOFT_SIGNAL, i64::from(libc::SIGUSR1)]),
            StopEvent::Signal {
                thread,
                signal: libc::SIGUSR1
            }
        );
    }

    #[test]
    fn software_exception_other_than_a_soft_signal_holds_no_signal()
    {
        // Issue #244: these used to decode as `Signal(SIGTRAP)`, which no soft-signal
        // exception backs, so a forwarding continue was refused.
        let thread = ThreadId::from(THREAD);
        // `EXC_UNIX_BAD_SYSCALL`, `EXC_UNIX_BAD_PIPE`, `EXC_UNIX_ABORT`, and a code of no
        // known meaning.
        for code in [0x10000, 0x10001, 0x10002, 7] {
            let stop = event(EXC_SOFTWARE, None, [code, 0]);
            assert!(
                matches!(&stop, StopEvent::Other { thread: Some(t), reason } if *t == thread && reason.contains("EXC_SOFTWARE")),
                "code {code:#x}: {stop:?}"
            );
            assert_eq!(stop.deliverable_signal(), None, "code {code:#x}");
        }
    }
}
