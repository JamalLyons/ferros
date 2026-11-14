//! Hold threads born during a default-`Suspend` resume.
//!
//! XNU's `thread_create_running` starts a pthread with no user suspend count, so a
//! snapshot of `task_threads` cannot keep a thread created after `task_resume` stopped.
//! Before such a resume, the backend plants a `brk` on the first instruction of
//! `libsystem_pthread`'s `_thread_start` and `_start_wqthread`. The exception loop
//! `thread_suspend`s the newborn and answers the breakpoint. The original instruction
//! is restored before that thread is allowed to run. Every other thread stays
//! suspended until that instruction has completed and the trap is back, so none of
//! them can pass the restored entry. A continue of every thread restores the bytes
//! first and does not single-step.
//!
//! Addresses come from the loaded image's own slice (UUID, CPU type, and CPU subtype).
//! Before dyld has published `libsystem_pthread`, the resume breakpoints the
//! executable entry and arms those pthread entries when that entry is reached,
//! before the original instruction runs. A one-instruction step may resume when
//! the entry cannot be located yet. Once the image is published, failing to
//! install the traps fails the resume. A software breakpoint at an armed entry
//! is reported to the client, and that newborn stays suspended.

use std::fs;

use ferros_core::backend::StopEvent;
use ferros_core::types::{Address, Arm64Register, ProcessId, RegisterId};
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
    /// The engine planted a software breakpoint on this instruction.
    client_breakpoint: bool,
    /// Newborns are held here. The dyld image notifier is not an entry.
    entry: bool,
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
    /// Threads suspended so they cannot execute a birth site while its original
    /// instruction is temporarily restored. Released once the trap is armed again.
    frozen: Vec<thread_act_t>,
    /// Send rights taken for threads frozen from the exception loop, released on thaw.
    owned: Vec<thread_act_t>,
    /// Address of the one-shot executable-entry breakpoint.
    notifier_pc: Option<u64>,
    /// Debug-register slot kept so an older hardware watch can still be cleared.
    notifier_slot: Option<u32>,
}

impl BirthPolicy
{
    pub(crate) fn is_active(&self) -> bool
    {
        self.active
    }

    /// A `brk` at an armed pthread entry that the engine did not also breakpoint.
    /// Hardware single-step uses different codes and is never reported as
    /// [`StopEvent::Breakpoint`].
    pub(crate) fn is_birth_breakpoint(&self, event: &StopEvent) -> bool
    {
        self.matching_entry(event, false)
    }

    /// A `brk` at an armed pthread entry where the engine also has a breakpoint.
    /// The newborn is still held, and the stop is reported.
    pub(crate) fn is_reported_birth(&self, event: &StopEvent) -> bool
    {
        self.matching_entry(event, true)
    }

    /// Dyld calling `_lldb_image_notifier`. The image list may now contain pthread.
    pub(crate) fn is_notifier(&self, event: &StopEvent) -> bool
    {
        let StopEvent::Breakpoint { pc, .. } = event else {
            return false;
        };
        self.notifier_pc == Some(pc.value())
    }

    fn matching_entry(&self, event: &StopEvent, client_breakpoint: bool) -> bool
    {
        let StopEvent::Breakpoint { pc, .. } = event else {
            return false;
        };
        self.active
            && self.sites.iter().any(|site| {
                site.entry && site.armed && site.client_breakpoint == client_breakpoint && site.address == pc.value()
            })
    }

    pub(crate) fn armed_addresses(&self) -> Vec<u64>
    {
        self.sites
            .iter()
            .filter(|site| site.entry && site.armed)
            .map(|site| site.address)
            .collect()
    }

    /// A 4-byte write exactly on an armed trap.
    ///
    /// Returns the bytes that must remain in memory (`brk`) when this policy handles
    /// the write.
    pub(crate) fn note_engine_write(&mut self, address: u64, bytes: &[u8; 4]) -> Option<[u8; 4]>
    {
        let site = self.sites.iter_mut().find(|site| site.armed && site.address == address)?;
        if *bytes == BRK {
            site.client_breakpoint = true;
        } else if *bytes == site.original {
            site.client_breakpoint = false;
        } else {
            site.original = *bytes;
            site.client_breakpoint = false;
        }
        Some(BRK)
    }

    pub(crate) fn is_step_off(&self, event: &StopEvent) -> bool
    {
        let StopEvent::SingleStep { thread } = event else {
            return false;
        };
        self.step_off.iter().any(|port| u64::from(*port) == thread.raw())
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

    pub(crate) fn freeze(&mut self, thread: thread_act_t)
    {
        if !self.frozen.contains(&thread) {
            self.frozen.push(thread);
        }
    }

    pub(crate) fn take_frozen(&mut self) -> Vec<thread_act_t>
    {
        std::mem::take(&mut self.frozen)
    }

    pub(crate) fn take_owned(&mut self, resumed: &[thread_act_t]) -> Vec<thread_act_t>
    {
        let mut dropped = Vec::new();
        self.owned.retain(|port| {
            if resumed.contains(port) {
                dropped.push(*port);
                false
            } else {
                true
            }
        });
        dropped
    }

    /// Install traps when they are not already armed.
    ///
    /// `require_cover` is set when some thread will `Continue`. A one-instruction
    /// step may resume before dyld is locatable; a continue may not.
    pub(crate) fn arm(
        &mut self,
        task: mach_port_t,
        pid: ProcessId,
        threads: &[thread_act_t],
        require_cover: bool,
    ) -> Result<(), BirthInstallError>
    {
        let _ = threads;
        if !self.sites.iter().any(|site| site.entry) {
            self.sites.extend(resolve_sites(task, pid)?);
        }
        if self.sites.iter().any(|site| site.entry) {
            self.clear_notifier_hardware(threads);
            self.forget_notifier();
            self.patch_unarmed(task)?;
            self.active = true;
            return Ok(());
        }
        if !self.sites.iter().any(|site| !site.entry)
            && let Some(bridge) = resolve_main_entry(task, pid)?
        {
            self.notifier_pc = Some(bridge.address);
            self.sites.push(bridge);
        }
        if self.sites.iter().any(|site| !site.entry) {
            self.patch_unarmed_bridge(task)?;
            self.active = true;
            return Ok(());
        }
        if require_cover {
            return Err(BirthInstallError::Unsupported(
                "libsystem_pthread is not loaded and the executable entry is unavailable; new threads cannot be suspended"
                    .into(),
            ));
        }
        self.active = false;
        Ok(())
    }

    /// Arm pthread entries if dyld has published them, and drop the notifier.
    ///
    /// Returns whether entries are now armed.
    pub(crate) fn install_entries(
        &mut self,
        task: mach_port_t,
        pid: ProcessId,
        notifier: thread_act_t,
    ) -> Result<bool, BirthInstallError>
    {
        if !self.sites.iter().any(|site| site.entry) {
            let mut entries = resolve_sites(task, pid)?;
            if entries.is_empty()
                && let Some(load) = pthread_load_in_notification(task, notifier)
            {
                entries = resolve_pthread_at(task, load)?;
            }
            self.sites.extend(entries);
        }
        if !self.sites.iter().any(|site| site.entry) {
            return Ok(false);
        }
        self.retire_notifier(task)?;
        self.patch_unarmed(task)?;
        self.active = true;
        Ok(true)
    }

    fn patch_unarmed(&mut self, task: mach_port_t) -> Result<(), BirthInstallError>
    {
        let mut newly_armed: Vec<usize> = Vec::new();
        for index in 0..self.sites.len() {
            if self.sites[index].armed || !self.sites[index].entry {
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
        Ok(())
    }

    fn retire_notifier(&mut self, task: mach_port_t) -> Result<(), BirthInstallError>
    {
        let Some(index) = self.sites.iter().position(|site| !site.entry) else {
            return Ok(());
        };
        if self.sites[index].armed {
            let address = self.sites[index].address;
            let original = self.sites[index].original;
            patch(task, address, &original).map_err(BirthInstallError::Os)?;
        }
        self.sites.remove(index);
        Ok(())
    }

    fn patch_unarmed_bridge(&mut self, task: mach_port_t) -> Result<(), BirthInstallError>
    {
        for index in 0..self.sites.len() {
            if self.sites[index].entry || self.sites[index].armed {
                continue;
            }
            patch(task, self.sites[index].address, &BRK).map_err(BirthInstallError::Os)?;
            self.sites[index].armed = true;
        }
        Ok(())
    }

    pub(crate) fn clear_notifier_hardware(&mut self, threads: &[thread_act_t])
    {
        let Some(slot) = self.notifier_slot.take() else {
            return;
        };
        for &thread in threads {
            if let Err(err) = crate::registers::clear_hardware_breakpoint(thread, slot) {
                tracing::debug!("clearing the image-notifier breakpoint on {thread} failed: {err}");
            }
        }
        if let Some(site) = self.sites.iter_mut().find(|site| !site.entry) {
            site.armed = false;
        }
    }

    pub(crate) fn bridge_address(&self) -> Option<u64>
    {
        self.notifier_pc
    }

    pub(crate) fn forget_notifier(&mut self)
    {
        self.notifier_pc = None;
        self.sites.retain(|site| site.entry);
    }

    /// Put every original pthread instruction back. A later suspend resume may arm again.
    pub(crate) fn disarm(&mut self, task: mach_port_t) -> Result<(), String>
    {
        self.active = false;
        let mut failure = None;
        for site in &mut self.sites {
            if !site.armed || !site.entry {
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
    ///
    /// # Errors
    ///
    /// Returns the first failed write. Later sites are still attempted.
    pub(crate) fn rearm_except(&mut self, task: mach_port_t, occupied: &[u64]) -> Result<(), String>
    {
        if !self.active {
            return Ok(());
        }
        let mut failure = None;
        for site in &mut self.sites {
            if site.armed || occupied.contains(&site.address) {
                continue;
            }
            match patch(task, site.address, &BRK) {
                Ok(()) => site.armed = true,
                Err(err) => {
                    failure.get_or_insert(err);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub(crate) fn rearm_all(&mut self, task: mach_port_t)
    {
        if let Err(err) = self.rearm_except(task, &[]) {
            tracing::error!("failed to restore the new-thread trap: {err}");
        }
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

fn pthread_load_in_notification(task: mach_port_t, thread: thread_act_t) -> Option<u64>
{
    let registers = crate::task::read_registers_for_port(ferros_core::types::Architecture::Arm64, thread).ok()?;
    let count = registers.get(RegisterId::Arm64(Arm64Register::X(1)))?;
    let infos = registers.get(RegisterId::Arm64(Arm64Register::X(2)))?;
    if count == 0 || count > 4096 || infos == 0 {
        return None;
    }
    for index in 0..count {
        let record = infos.checked_add(index.checked_mul(24)?)?;
        let bytes = read_target(task, record, 16).ok()?;
        if bytes.len() < 16 {
            return None;
        }
        let load = u64::from_le_bytes(bytes[..8].try_into().ok()?);
        let path = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        if path_ends_with_pthread(task, path) {
            return Some(load);
        }
    }
    None
}

fn path_ends_with_pthread(task: mach_port_t, address: u64) -> bool
{
    let Ok(bytes) = read_target(task, address, 128) else {
        return false;
    };
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    bytes[..end].ends_with(b"libsystem_pthread.dylib")
}

fn resolve_pthread_at(task: mach_port_t, load: u64) -> Result<Vec<BirthSite>, BirthInstallError>
{
    let identity = image_identity(task, load).map_err(BirthInstallError::Os)?;
    if identity.cputype != CPU_TYPE_ARM64 {
        return Ok(Vec::new());
    }
    let file_bytes = fs::read("/usr/lib/system/libsystem_pthread.dylib")
        .map_err(|err| BirthInstallError::Os(format!("reading libsystem_pthread failed: {err}")))?;
    let slice = matching_slice(&file_bytes, &identity).map_err(BirthInstallError::Os)?;
    sites_from_slice(task, slice, load)
}

fn resolve_main_entry(task: mach_port_t, pid: ProcessId) -> Result<Option<BirthSite>, BirthInstallError>
{
    let images = crate::images::loaded_images(task, pid).map_err(|err| {
        BirthInstallError::Os(format!(
            "dyld image list is unreadable ({err}); new threads cannot be suspended"
        ))
    })?;
    let Some(image) = images.first() else {
        return Ok(None);
    };
    let load = image.load_address.value();
    let header = read_target(task, load, 32).map_err(BirthInstallError::Os)?;
    if header.len() < 32 || u32::from_le_bytes(header[..4].try_into().expect("magic")) != MH_MAGIC_64 {
        return Ok(None);
    }
    let ncmds = u32::from_le_bytes(header[16..20].try_into().expect("ncmds")) as usize;
    let sizeofcmds = u32::from_le_bytes(header[20..24].try_into().expect("sizeofcmds")) as usize;
    if sizeofcmds > MAX_COMMANDS {
        return Err(BirthInstallError::Os(format!(
            "executable load commands are {sizeofcmds} bytes"
        )));
    }
    let bytes = read_target(task, load, 32 + sizeofcmds).map_err(BirthInstallError::Os)?;
    let entryoff = main_entryoff(&bytes, ncmds, sizeofcmds).map_err(BirthInstallError::Os)?;
    let Some(address) = load.checked_add(entryoff) else {
        return Err(BirthInstallError::Os("executable entry overflows".into()));
    };
    let original = read_word(task, address).map_err(BirthInstallError::Os)?;
    Ok(Some(BirthSite {
        address,
        original,
        armed: false,
        client_breakpoint: original == BRK,
        entry: false,
    }))
}

const LC_MAIN: u32 = 0x8000_0028;

fn main_entryoff(bytes: &[u8], ncmds: usize, sizeofcmds: usize) -> Result<u64, String>
{
    let end = 32 + sizeofcmds;
    if bytes.len() < end {
        return Err("truncated executable load commands".into());
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
        if cmd == LC_MAIN {
            if cmdsize < 16 {
                return Err("LC_MAIN is truncated".into());
            }
            return Ok(u64::from_le_bytes(
                bytes[offset + 8..offset + 16].try_into().expect("entryoff"),
            ));
        }
        offset += cmdsize;
    }
    Err("executable has no LC_MAIN".into())
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
        // pthread is not in the dyld list yet. The caller arms `_lldb_image_notifier`
        // so the entries can be trapped when dyld publishes the library.
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
    sites_from_slice(task, slice, image.load_address.value())
}

fn sites_from_slice(task: mach_port_t, slice: &[u8], load: u64) -> Result<Vec<BirthSite>, BirthInstallError>
{
    let file =
        File::parse(slice).map_err(|err| BirthInstallError::Os(format!("parsing libsystem_pthread failed: {err}")))?;
    let base = file.relative_address_base();
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
        let client_breakpoint = live == BRK;
        if live != original && !client_breakpoint {
            return Err(BirthInstallError::Os(format!(
                "{name} at {runtime:#x} is {live:02x?} in the process and {original:02x?} in the loaded slice"
            )));
        }
        sites.push(BirthSite {
            address: runtime,
            original,
            armed: false,
            client_breakpoint,
            entry: true,
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
