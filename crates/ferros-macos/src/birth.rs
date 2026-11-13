//! Hold threads born during a default-`Suspend` resume.
//!
//! XNU's `thread_create_running` starts a pthread with no user suspend count, so a
//! snapshot of `task_threads` cannot keep a thread created after `task_resume` stopped.
//! Before such a resume, the backend plants a `brk` on the first instruction of
//! `libsystem_pthread`'s `_thread_start` and `_start_wqthread`. The exception loop
//! `thread_suspend`s the newborn, answers the breakpoint, and leaves every other thread
//! running. The original instruction is restored before that thread is allowed to run:
//! a later continue with the trap still armed single-steps off the site, and a continue
//! of every thread restores the bytes first.
//!
//! Addresses come from the loaded image's own slice (UUID, CPU type, and CPU subtype).
//! Before dyld has published `libsystem_pthread`, a resume that needs this policy fails
//! instead of falling back to the snapshot.

use std::fs;

use ferros_core::backend::StopEvent;
use ferros_core::types::{Address, ProcessId};
use libc::{mach_port_t, thread_act_t};
use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
use object::{File, FileKind, Object, ObjectSection, ObjectSymbol};

use crate::memory::{MemoryProtectionGuard, read_memory, write_memory};

/// `brk #0`, the same trap the engine plants for software breakpoints.
const BRK: [u8; 4] = [0x00, 0x00, 0x20, 0xD4];

const MH_MAGIC_64: u32 = 0xfeed_facf;
const LC_UUID: u32 = 0x1b;
/// Capability bits live in the top byte of a Mach-O CPU subtype (`CPU_SUBTYPE_MASK`).
const CPU_SUBTYPE_MASK: u32 = 0xff00_0000;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
/// Load commands larger than this are rejected instead of being trusted from the target.
const MAX_COMMANDS: usize = 1 << 20;
const PTHREAD_FILE: &str = "libsystem_pthread.dylib";
const ENTRY_SYMBOLS: [&str; 2] = ["_thread_start", "_start_wqthread"];

/// Why a birth trap could not be installed. The resume that needed it must fail.
#[derive(Debug)]
pub(crate) enum BirthInstallError
{
    /// The target cannot be covered (dyld has not published pthread, or it is not arm64).
    Unsupported(String),
    /// The traps could not be written, or the loaded bytes do not match the file slice.
    Os(String),
}

/// One userspace instruction a new pthread executes before any application code.
#[derive(Debug, Clone)]
struct BirthSite
{
    address: u64,
    original: [u8; 4],
    armed: bool,
}

/// Traps and holds for threads created while the default action is `Suspend`.
#[derive(Debug, Default)]
pub(crate) struct BirthPolicy
{
    /// New threads that enter an armed site are held. A full continue clears this.
    active: bool,
    sites: Vec<BirthSite>,
    /// Newborns `thread_suspend`ed by the exception loop, one count each.
    /// The backend absorbs these into `suspended_threads` and resumes them once.
    pub(crate) suspended: Vec<thread_act_t>,
    /// Threads whose next hardware step only leaves a birth site. That stop is not
    /// reported: the trap is restored and the thread keeps running.
    step_off: Vec<thread_act_t>,
}

impl BirthPolicy
{
    pub(crate) fn is_active(&self) -> bool
    {
        self.active
    }

    /// A `brk` at an armed birth site. Hardware single-step uses different codes and
    /// is never reported as [`StopEvent::Breakpoint`].
    pub(crate) fn is_birth_breakpoint(&self, event: &StopEvent) -> bool
    {
        let StopEvent::Breakpoint { pc, .. } = event else {
            return false;
        };
        self.active && self.sites.iter().any(|site| site.armed && site.address == pc.value())
    }

    pub(crate) fn is_step_off(&self, event: &StopEvent) -> bool
    {
        let StopEvent::SingleStep { thread } = event else {
            return false;
        };
        self.step_off.iter().any(|port| u64::from(*port) == thread.raw())
    }

    pub(crate) fn armed_addresses(&self) -> Vec<u64>
    {
        self.sites.iter().filter(|site| site.armed).map(|site| site.address).collect()
    }

    pub(crate) fn hold(&mut self, thread: thread_act_t)
    {
        if !self.suspended.contains(&thread) {
            self.suspended.push(thread);
        }
    }

    pub(crate) fn note_step_off(&mut self, thread: thread_act_t)
    {
        if !self.step_off.contains(&thread) {
            self.step_off.push(thread);
        }
    }

    pub(crate) fn take_step_off(&mut self, thread: thread_act_t) -> bool
    {
        let before = self.step_off.len();
        self.step_off.retain(|port| *port != thread);
        self.step_off.len() != before
    }

    pub(crate) fn step_off_threads(&self) -> Vec<thread_act_t>
    {
        self.step_off.clone()
    }

    pub(crate) fn clear_step_offs(&mut self) -> Vec<thread_act_t>
    {
        std::mem::take(&mut self.step_off)
    }

    /// Install traps when they are not already armed. Symbols are resolved once
    /// the pthread image is published. Before that, pthread entries are not mapped,
    /// so a suspend resume proceeds with no traps and the policy stays inactive.
    pub(crate) fn arm(&mut self, task: mach_port_t, pid: ProcessId) -> Result<(), BirthInstallError>
    {
        if self.sites.is_empty() {
            self.sites = resolve_sites(task, pid)?;
        }
        if self.sites.is_empty() {
            self.active = false;
            return Ok(());
        }
        let mut newly_armed: Vec<usize> = Vec::new();
        for index in 0..self.sites.len() {
            if self.sites[index].armed {
                continue;
            }
            if let Err(err) = patch(task, self.sites[index].address, &BRK) {
                for armed in newly_armed {
                    let original = self.sites[armed].original;
                    let address = self.sites[armed].address;
                    if patch(task, address, &original).is_ok() {
                        self.sites[armed].armed = false;
                    }
                }
                return Err(BirthInstallError::Os(err));
            }
            self.sites[index].armed = true;
            newly_armed.push(index);
        }
        self.active = true;
        Ok(())
    }

    /// Put every original instruction back. A later suspend resume may arm the same sites.
    pub(crate) fn disarm(&mut self, task: mach_port_t) -> Result<(), String>
    {
        self.active = false;
        let mut failure = None;
        for site in &mut self.sites {
            if !site.armed {
                continue;
            }
            match patch(task, site.address, &site.original) {
                Ok(()) => site.armed = false,
                Err(err) => {
                    failure.get_or_insert(err);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(crate) fn disarm_address(&mut self, task: mach_port_t, address: u64) -> Result<(), String>
    {
        let Some(index) = self.sites.iter().position(|site| site.address == address) else {
            return Ok(());
        };
        let original = self.sites[index].original;
        patch(task, address, &original)?;
        self.sites[index].armed = false;
        Ok(())
    }

    /// Write `brk` back onto sites nobody still has to execute.
    pub(crate) fn rearm_except(&mut self, task: mach_port_t, occupied: &[u64])
    {
        if !self.active {
            return;
        }
        for site in &mut self.sites {
            if site.armed || occupied.contains(&site.address) {
                continue;
            }
            if patch(task, site.address, &BRK).is_ok() {
                site.armed = true;
            } else {
                tracing::error!("failed to restore the new-thread trap at {:#x}", site.address);
            }
        }
    }

    pub(crate) fn rearm_all(&mut self, task: mach_port_t)
    {
        self.rearm_except(task, &[]);
    }
}

fn patch(task: mach_port_t, address: u64, bytes: &[u8; 4]) -> Result<(), String>
{
    let at = Address::from(address);
    match write_memory(task, at, bytes) {
        Ok(_) => Ok(()),
        Err(_) => {
            let guard = MemoryProtectionGuard::make_writable(task, at, bytes.len()).map_err(|err| err.to_string())?;
            let result = write_memory(task, at, bytes).map(|_| ());
            drop(guard);
            result.map_err(|err| err.to_string())
        }
    }
}

fn resolve_sites(task: mach_port_t, pid: ProcessId) -> Result<Vec<BirthSite>, BirthInstallError>
{
    let images = crate::images::loaded_images(task, pid).map_err(|err| {
        BirthInstallError::Unsupported(format!(
            "dyld image list is unreadable ({err}); new threads cannot be suspended"
        ))
    })?;
    let Some(image) = images
        .iter()
        .find(|image| image.path.file_name().is_some_and(|name| name == PTHREAD_FILE))
    else {
        // pthread_create cannot run until this image is mapped. Leave the policy
        // inactive so an entry-point step, before dyld publishes the library, still resumes.
        return Ok(Vec::new());
    };
    let identity = image_identity(task, image.load_address.value()).map_err(BirthInstallError::Os)?;
    if identity.cputype != CPU_TYPE_ARM64 {
        return Err(BirthInstallError::Unsupported(format!(
            "libsystem_pthread is not an arm64 image (cputype {:#x}); new-thread suspension is not implemented for this \
             process",
            identity.cputype
        )));
    }
    let file_bytes = fs::read(&image.path)
        .map_err(|err| BirthInstallError::Os(format!("reading {} failed: {err}", image.path.display())))?;
    let slice = matching_slice(&file_bytes, &identity).map_err(BirthInstallError::Os)?;
    let file =
        File::parse(slice).map_err(|err| BirthInstallError::Os(format!("parsing libsystem_pthread failed: {err}")))?;
    let base = file.relative_address_base();
    let load = image.load_address.value();
    let slide = load
        .checked_sub(base)
        .ok_or_else(|| BirthInstallError::Os(format!("pthread load address {load:#x} is below its image base {base:#x}")))?;
    let mut sites = Vec::with_capacity(ENTRY_SYMBOLS.len());
    for name in ENTRY_SYMBOLS {
        let vm = defined_symbol(&file, name).map_err(BirthInstallError::Os)?;
        let runtime = vm
            .checked_add(slide)
            .ok_or_else(|| BirthInstallError::Os(format!("{name} overflows after the dyld slide")))?;
        let original = file_instruction(&file, vm)
            .ok_or_else(|| BirthInstallError::Os(format!("{name} has no mapped instruction bytes in libsystem_pthread")))?;
        let live = read_word(task, runtime).map_err(BirthInstallError::Os)?;
        if live != original {
            return Err(BirthInstallError::Os(format!(
                "{name} at {runtime:#x} is {live:02x?} in the process and {original:02x?} in the loaded slice"
            )));
        }
        sites.push(BirthSite {
            address: runtime,
            original,
            armed: false,
        });
    }
    Ok(sites)
}

struct ImageIdentity
{
    cputype: u32,
    cpusubtype: u32,
    uuid: [u8; 16],
}

fn image_identity(task: mach_port_t, load_address: u64) -> Result<ImageIdentity, String>
{
    let header = read_target(task, load_address, 32)?;
    if header.len() < 32 || u32::from_le_bytes(header[..4].try_into().expect("32 bytes")) != MH_MAGIC_64 {
        return Err(format!("libsystem_pthread at {load_address:#x} has no 64-bit Mach-O header"));
    }
    let cputype = u32::from_le_bytes(header[4..8].try_into().expect("cputype"));
    let cpusubtype = u32::from_le_bytes(header[8..12].try_into().expect("cpusubtype"));
    let ncmds = u32::from_le_bytes(header[16..20].try_into().expect("ncmds")) as usize;
    let sizeofcmds = u32::from_le_bytes(header[20..24].try_into().expect("sizeofcmds")) as usize;
    if sizeofcmds > MAX_COMMANDS {
        return Err(format!("libsystem_pthread load commands are {sizeofcmds} bytes"));
    }
    let bytes = read_target(task, load_address, 32 + sizeofcmds)?;
    let uuid = walk_uuid(&bytes, ncmds, sizeofcmds)?;
    Ok(ImageIdentity {
        cputype,
        cpusubtype: cpusubtype & !CPU_SUBTYPE_MASK,
        uuid,
    })
}

fn walk_uuid(bytes: &[u8], ncmds: usize, sizeofcmds: usize) -> Result<[u8; 16], String>
{
    let end = 32 + sizeofcmds;
    if bytes.len() < end {
        return Err("truncated libsystem_pthread load commands".into());
    }
    let mut offset = 32;
    for _ in 0..ncmds {
        if offset + 8 > end {
            return Err("truncated Mach-O load command".into());
        }
        let cmd = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("cmd"));
        let cmdsize = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().expect("cmdsize")) as usize;
        if cmdsize < 8 || offset + cmdsize > end {
            return Err("Mach-O load command exceeds the header".into());
        }
        if cmd == LC_UUID {
            if cmdsize < 24 {
                return Err("LC_UUID is truncated".into());
            }
            let mut uuid = [0; 16];
            uuid.copy_from_slice(&bytes[offset + 8..offset + 24]);
            return Ok(uuid);
        }
        offset += cmdsize;
    }
    Err("libsystem_pthread has no LC_UUID".into())
}

fn matching_slice<'a>(bytes: &'a [u8], identity: &ImageIdentity) -> Result<&'a [u8], String>
{
    let mut found = None;
    for slice in macho_slices(bytes)? {
        let file = File::parse(slice).map_err(|err| format!("parsing a libsystem_pthread slice failed: {err}"))?;
        let Some(uuid) = file
            .mach_uuid()
            .map_err(|err| format!("reading a slice UUID failed: {err}"))?
        else {
            continue;
        };
        if uuid != identity.uuid {
            continue;
        }
        let header = slice.get(..12).ok_or("slice is shorter than a Mach-O header")?;
        let cputype = u32::from_le_bytes(header[4..8].try_into().expect("cputype"));
        let cpusubtype = u32::from_le_bytes(header[8..12].try_into().expect("cpusubtype")) & !CPU_SUBTYPE_MASK;
        if cputype != identity.cputype || cpusubtype != identity.cpusubtype {
            return Err(format!(
                "libsystem_pthread UUID matches a slice whose CPU type {cputype:#x} subtype {cpusubtype:#x} is not the \
                 loaded image"
            ));
        }
        if found.is_some() {
            return Err("libsystem_pthread UUID matched more than one slice".into());
        }
        found = Some(slice);
    }
    found.ok_or_else(|| "no libsystem_pthread slice matches the loaded image UUID".into())
}

fn macho_slices(bytes: &[u8]) -> Result<Vec<&[u8]>, String>
{
    match FileKind::parse(bytes).map_err(|err| format!("unrecognized libsystem_pthread file: {err}"))? {
        FileKind::MachOFat32 => {
            let fat = MachOFatFile32::parse(bytes).map_err(|err| format!("parsing the 32-bit fat header failed: {err}"))?;
            fat.arches()
                .iter()
                .map(|arch| arch.data(bytes).map_err(|err| format!("reading a fat slice failed: {err}")))
                .collect()
        }
        FileKind::MachOFat64 => {
            let fat = MachOFatFile64::parse(bytes).map_err(|err| format!("parsing the 64-bit fat header failed: {err}"))?;
            fat.arches()
                .iter()
                .map(|arch| arch.data(bytes).map_err(|err| format!("reading a fat slice failed: {err}")))
                .collect()
        }
        _ => Ok(vec![bytes]),
    }
}

fn defined_symbol(file: &File<'_>, name: &str) -> Result<u64, String>
{
    let mut found = None;
    for symbol in file.symbols() {
        let Ok(symbol_name) = symbol.name() else {
            continue;
        };
        if symbol_name != name || !symbol.is_definition() || symbol.address() == 0 {
            continue;
        }
        if found.is_some() {
            return Err(format!("libsystem_pthread defines {name} more than once"));
        }
        found = Some(symbol.address());
    }
    found.ok_or_else(|| format!("libsystem_pthread does not define {name}"))
}

fn file_instruction(file: &File<'_>, vm: u64) -> Option<[u8; 4]>
{
    for section in file.sections() {
        let Ok(data) = section.data() else {
            continue;
        };
        let start = section.address();
        let Some(offset) = vm.checked_sub(start) else {
            continue;
        };
        let Ok(offset) = usize::try_from(offset) else {
            continue;
        };
        let Some(bytes) = data.get(offset..offset + 4) else {
            continue;
        };
        return bytes.try_into().ok();
    }
    None
}

fn read_word(task: mach_port_t, address: u64) -> Result<[u8; 4], String>
{
    let bytes = read_target(task, address, 4)?;
    bytes.as_slice().try_into().map_err(|_| format!("short read at {address:#x}"))
}

fn read_target(task: mach_port_t, address: u64, len: usize) -> Result<Vec<u8>, String>
{
    let bytes = read_memory(task, Address::from(address), len)
        .map_err(|err| format!("reading {len} bytes at {address:#x} failed: {err}"))?;
    if bytes.len() == len {
        Ok(bytes)
    } else {
        Err(format!("reading {len} bytes at {address:#x} returned {}", bytes.len()))
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    /// On this machine both pthread entries are `stp xzr, xzr, [sp, #-16]!` then `bl`.
    /// The debugger must execute that `stp`; this locks the bytes the slice actually contains.
    #[test]
    fn pthread_entries_are_stp_then_bl()
    {
        const STP_XZR_XZR_PREDEC: [u8; 4] = [0xff, 0x7f, 0xbf, 0xa9];
        let bytes = fs::read("/usr/lib/system/libsystem_pthread.dylib").expect("pthread dylib");
        let slices = macho_slices(&bytes).expect("slices");
        assert!(slices.len() > 1, "expected a fat libsystem_pthread");
        for slice in slices {
            let file = File::parse(slice).expect("slice");
            if file.architecture() != object::Architecture::Aarch64 {
                continue;
            }
            for name in ENTRY_SYMBOLS {
                let vm = defined_symbol(&file, name).unwrap_or_else(|err| panic!("{name}: {err}"));
                let first = file_instruction(&file, vm).unwrap_or_else(|| panic!("{name} first instruction"));
                let second = file_instruction(&file, vm + 4).unwrap_or_else(|| panic!("{name} second instruction"));
                assert_eq!(first, STP_XZR_XZR_PREDEC, "{name} first instruction changed");
                assert_eq!(second[3] & 0xfc, 0x94, "{name} second instruction is not bl: {second:02x?}");
            }
        }
    }

    #[test]
    fn uuid_walk_rejects_a_truncated_header()
    {
        assert!(walk_uuid(&[0; 32], 1, 16).is_err());
    }
}
