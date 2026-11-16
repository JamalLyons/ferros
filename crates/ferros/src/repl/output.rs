//! Text formatting shared by the commands.
//!
//! Every line is plain text without colour or timestamps, so batch output is
//! deterministic and greppable. Addresses print as `0x` followed by lowercase hex
//! without padding, except in the hexdump, whose row addresses are padded to 16 digits.

use std::fmt::Write as _;

use ferros_core::{Address, Architecture, Registers, Session, StopEvent};

/// Bytes shown by `memory` when no length is given.
pub const DEFAULT_MEMORY_LEN: usize = 64;

/// Bytes per hexdump row.
pub const HEXDUMP_ROW: usize = 16;

/// One line describing `event`.
///
/// For a breakpoint stop the breakpoint's id is looked up in the session's table; a
/// single-step stop reads the stepped thread's pc. Both reads are best effort.
pub fn describe_stop(session: &mut Session, event: &StopEvent) -> String
{
    match event {
        StopEvent::Breakpoint { thread, pc } => {
            let id = session
                .breakpoints()
                .iter()
                .find(|info| info.address == *pc)
                .map(|info| info.id.raw());
            match id {
                Some(id) => format!("stopped: breakpoint {id} at {:#x} (thread {})", pc.value(), thread.raw()),
                None => format!("stopped: trap at {:#x} (thread {})", pc.value(), thread.raw()),
            }
        }
        StopEvent::SingleStep { thread } => match session.read_registers_for(*thread) {
            Ok(registers) => format!("stopped: step at {:#x} (thread {})", registers.pc.value(), thread.raw()),
            Err(_) => format!("stopped: step (thread {})", thread.raw()),
        },
        StopEvent::Signal { thread, signal } => format!("stopped: signal {signal} (thread {})", thread.raw()),
        StopEvent::Interrupted { thread } => match thread {
            Some(thread) => format!("stopped: interrupted (thread {})", thread.raw()),
            None => "stopped: interrupted".to_owned(),
        },
        StopEvent::Exited { code } => format!("process exited with status {code}"),
        StopEvent::Killed { signal } => format!("process killed by signal {signal}"),
        StopEvent::Other { thread, reason } => match thread {
            Some(thread) => format!("stopped: {reason} (thread {})", thread.raw()),
            None => format!("stopped: {reason}"),
        },
        other => format!("stopped: {other:?}"),
    }
}

/// The registers of one thread, one per line: `pc`, `sp`, `fp`, `lr` and `cpsr`, then
/// the general registers `x0`..`x28` (arm64; other architectures print `r0`.. for each
/// general register).
pub fn format_registers(registers: &Registers) -> String
{
    let mut text = String::new();
    let mut line = |name: &str, value: u64| {
        let _ = writeln!(text, "{name:<4} {value:#018x}");
    };
    line("pc", registers.pc.value());
    line("sp", registers.sp.value());
    line("fp", registers.fp.value());
    if registers.architecture() == Architecture::Arm64 {
        if let Some(&lr) = registers.general.get(30) {
            line("lr", lr);
        }
        line("cpsr", registers.status);
        for (index, value) in registers.general.iter().take(29).enumerate() {
            line(&format!("x{index}"), *value);
        }
    } else {
        line("flags", registers.status);
        for (index, value) in registers.general.iter().enumerate() {
            line(&format!("r{index}"), *value);
        }
    }
    text
}

/// A hexdump of `bytes` read at `start`: 16 bytes per row, the row address, the bytes
/// in hex and the printable ASCII characters (others as `.`).
///
/// ```text
/// 0x0000000000001000  48 65 6c 6c 6f 00 01 02 03 04 05 06 07 08 09 0a  |Hello...........|
/// ```
pub fn hexdump(start: Address, bytes: &[u8]) -> String
{
    let mut text = String::new();
    for (row, chunk) in bytes.chunks(HEXDUMP_ROW).enumerate() {
        let address = start.value().wrapping_add((row * HEXDUMP_ROW) as u64);
        let _ = write!(text, "{address:#018x} ");
        for column in 0..HEXDUMP_ROW {
            match chunk.get(column) {
                Some(byte) => {
                    let _ = write!(text, " {byte:02x}");
                }
                None => text.push_str("   "),
            }
        }
        text.push_str("  |");
        text.extend(chunk.iter().map(|&byte| {
            if byte.is_ascii_graphic() || byte == b' ' {
                char::from(byte)
            } else {
                '.'
            }
        }));
        text.push_str("|\n");
    }
    text
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn hexdump_formats_full_and_partial_rows()
    {
        let mut bytes = b"Hello".to_vec();
        bytes.extend(0u8..14);
        let text = hexdump(Address::new(0x1000), &bytes);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "0x0000000000001000  48 65 6c 6c 6f 00 01 02 03 04 05 06 07 08 09 0a  |Hello...........|",
                "0x0000000000001010  0b 0c 0d                                         |...|",
            ]
        );
    }

    #[test]
    fn hexdump_of_nothing_is_empty()
    {
        assert_eq!(hexdump(Address::new(0x10), &[]), "");
    }

    #[test]
    fn registers_list_arm64_names()
    {
        let mut registers = Registers::new().with_arch(Architecture::Arm64);
        registers.general = (0..31).collect();
        registers.pc = Address::new(0x1234);
        let text = format_registers(&registers);
        assert!(text.starts_with("pc   0x0000000000001234\n"), "{text}");
        assert!(text.contains("\nlr   0x000000000000001e\n"), "{text}");
        assert!(text.contains("\nx28  0x000000000000001c\n"), "{text}");
        assert!(!text.contains("x29"), "{text}");
    }
}
