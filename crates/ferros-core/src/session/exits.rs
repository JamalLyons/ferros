//! Non-local exits: private traps on the code through which a thread leaves frames
//! without returning from them, so an operation that waits for a frame to return sees
//! the thread leave it by another route.
//!
//! Two routes are trapped: the system unwinder resuming at a landing pad (a panic caught
//! further out) and `_longjmp` in `libsystem_platform` (`longjmp` and `siglongjmp` end in
//! a branch to it). A thread that hits one is single-stepped until it leaves that image
//! above the stack it entered with: it has then landed where the exit goes.

use super::Session;
use crate::backend::StopEvent;
use crate::breakpoints::{BreakpointId, BreakpointKind, BreakpointRequest};
use crate::error::Result;
use crate::symbols::symtab::SymtabLayout;
use crate::types::{Address, ThreadId};

/// Functions through which the unwinder transfers control to a landing pad:
/// `unw_resume` and, on current macOS, the variant `_Unwind_RaiseException` and
/// `_Unwind_Resume` call. Matched with every leading underscore removed: the shared
/// cache's symbol table names them `___unw_resume` (the internal alias), the exports
/// `_unw_resume`.
const RESUME_SYMBOLS: [&str; 2] = ["unw_resume", "unw_resume_with_frames_walked"];

/// Instructions single-stepped from an exit entry to its landing before giving up (an
/// unwinder resume takes a few dozen, `_longjmp` about twenty-five).
const MAX_EXIT_STEPS: u32 = 4096;

/// An image whose code a thread passes through to leave frames without returning.
struct ExitSource
{
    /// File name of the image.
    image: &'static str,
    /// Whether a raw symbol table name (leading underscores kept) is an entry to trap.
    entry: fn(&str) -> bool,
}

const SOURCES: [ExitSource; 2] = [
    ExitSource {
        image: "libunwind.dylib",
        entry: is_unwinder_resume,
    },
    ExitSource {
        image: "libsystem_platform.dylib",
        entry: is_long_jump,
    },
];

fn is_unwinder_resume(name: &str) -> bool
{
    RESUME_SYMBOLS.contains(&name.trim_start_matches('_'))
}

/// `_longjmp`, the leaf that restores a `jmp_buf`. `longjmp` (raw `_longjmp`) and
/// `siglongjmp` restore the signal state and branch to it, so trapping it covers all
/// three without stepping through their system calls.
fn is_long_jump(name: &str) -> bool
{
    name == "__longjmp"
}

/// One exit image's code and entries in one process.
#[derive(Clone)]
pub(super) struct ExitImage
{
    /// Index into `SOURCES`.
    source: usize,
    load_address: u64,
    code: SymtabLayout,
    entries: Vec<Address>,
}

/// How to undo the temporary use of a breakpoint site.
pub(super) enum SiteOwner
{
    Private,
    Disabled,
    User,
}

/// A private trap on an exit entry.
struct Hook
{
    address: Address,
    /// Index into `ExitHooks::images`.
    image: usize,
    id: BreakpointId,
    owner: SiteOwner,
}

/// An exit in progress: the thread hit an entry with `sp` and is being single-stepped
/// until it leaves the image.
struct Leaving
{
    sp: Address,
    image: usize,
    steps: u32,
}

/// The exit traps armed for one thread.
pub(super) struct ExitHooks
{
    thread: ThreadId,
    images: Vec<ExitImage>,
    hooks: Vec<Hook>,
    leaving: Option<Leaving>,
}

/// Where one instruction step of an exit left the thread.
pub(super) enum ExitStep
{
    /// Still inside the exit's image: step again.
    Stepping,
    /// Out of the image above the stack the exit started from.
    Landed
    {
        pc: Address, sp: Address
    },
    /// No landing within [`MAX_EXIT_STEPS`]; the exit is no longer followed.
    Lost,
}

impl ExitHooks
{
    /// Hooks of `thread` with nothing armed.
    pub(super) fn empty(thread: ThreadId) -> Self
    {
        Self {
            thread,
            images: Vec::new(),
            hooks: Vec::new(),
            leaving: None,
        }
    }

    pub(super) fn thread(&self) -> ThreadId
    {
        self.thread
    }

    /// Whether an exit of the thread is being stepped.
    pub(super) fn leaving(&self) -> bool
    {
        self.leaving.is_some()
    }

    /// Whether `address` is one of these private traps (not a user breakpoint's site).
    fn private_hook_at(&self, address: Address) -> bool
    {
        self.hooks
            .iter()
            .any(|hook| hook.address == address && !matches!(hook.owner, SiteOwner::User))
    }
}

impl Session
{
    /// Arm a software breakpoint at `address` for an internal operation, recording how to
    /// undo it.
    pub(super) fn take_site(&mut self, address: Address) -> Result<(BreakpointId, SiteOwner)>
    {
        let existing = self.breakpoints.id_for_kind(address, BreakpointKind::Software);
        let owner = match existing.and_then(|id| self.breakpoints.get(id)) {
            Some(entry) if entry.info.enabled => SiteOwner::User,
            Some(_) => SiteOwner::Disabled,
            None => SiteOwner::Private,
        };
        let id = self.add_breakpoint(BreakpointRequest::Software { address })?;
        self.enable_breakpoint(id)?;
        Ok((id, owner))
    }

    pub(super) fn release_site(&mut self, id: BreakpointId, owner: &SiteOwner) -> Result<()>
    {
        match owner {
            SiteOwner::Private => self.remove_breakpoint(id)?,
            SiteOwner::Disabled => self.disable_breakpoint(id)?,
            SiteOwner::User => {}
        }
        Ok(())
    }

    /// Trap every exit entry for `thread`. Images that are not loaded or whose symbols
    /// cannot be read are skipped (and retried by the next call). On an error the hooks
    /// already armed are in `exits`, for the caller to release.
    pub(super) fn arm_exit_hooks(&mut self, exits: &mut Option<ExitHooks>, thread: ThreadId) -> Result<()>
    {
        let images = self.exit_images();
        let entries: Vec<(usize, Address)> = images
            .iter()
            .enumerate()
            .flat_map(|(image, found)| found.entries.iter().map(move |&address| (image, address)))
            .collect();
        let exits = exits.insert(ExitHooks {
            thread,
            images,
            hooks: Vec::new(),
            leaving: None,
        });
        for (image, address) in entries {
            let (id, owner) = self.take_site(address)?;
            exits.hooks.push(Hook {
                address,
                image,
                id,
                owner,
            });
        }
        Ok(())
    }

    /// Release every hook in `exits`, even when one fails; the first failure is returned.
    pub(super) fn release_exit_hooks(&mut self, exits: Option<ExitHooks>) -> Result<()>
    {
        let mut result = Ok(());
        for hook in exits.into_iter().flat_map(|exits| exits.hooks) {
            let released = self.release_site(hook.id, &hook.owner);
            if result.is_ok() {
                result = released;
            }
        }
        result
    }

    /// The exit image index whose private hook `event` hit, if it is a breakpoint stop of
    /// any thread at one of the hooks.
    pub(super) fn exit_hook_hit(&self, exits: &ExitHooks, event: &StopEvent) -> Result<Option<(ThreadId, usize)>>
    {
        let StopEvent::Breakpoint { thread, pc } = *event else {
            return Ok(None);
        };
        let address = self.arch()?.pc_after_trap(pc);
        if !exits.private_hook_at(address) {
            return Ok(None);
        }
        Ok(exits
            .hooks
            .iter()
            .find(|hook| hook.address == address)
            .map(|hook| (thread, hook.image)))
    }

    /// Start following an exit of the hooks' thread, which is stopped at an entry of
    /// image `image`. The caller single-steps the thread.
    pub(super) fn begin_exit(&mut self, exits: &mut ExitHooks, image: usize) -> Result<()>
    {
        let sp = self.backend.read_registers(exits.thread)?.sp;
        exits.leaving = Some(Leaving { sp, image, steps: 0 });
        Ok(())
    }

    /// One instruction step of a followed exit completed.
    pub(super) fn exit_stepped(&mut self, exits: &mut ExitHooks) -> Result<ExitStep>
    {
        let registers = self.backend.read_registers(exits.thread)?;
        let Some(leaving) = &mut exits.leaving else {
            return Ok(ExitStep::Lost);
        };
        let inside = exits
            .images
            .get(leaving.image)
            .is_some_and(|image| image.code.contains(registers.pc));
        if !inside && registers.sp > leaving.sp {
            exits.leaving = None;
            return Ok(ExitStep::Landed {
                pc: registers.pc,
                sp: registers.sp,
            });
        }
        if leaving.steps < MAX_EXIT_STEPS {
            leaving.steps += 1;
            return Ok(ExitStep::Stepping);
        }
        tracing::debug!("no landing within {MAX_EXIT_STEPS} instructions of a non-local exit");
        exits.leaving = None;
        Ok(ExitStep::Lost)
    }

    /// The process's exit images, read from their symbol tables in the target's memory.
    /// Each is cached once found.
    fn exit_images(&mut self) -> Vec<ExitImage>
    {
        if let Err(error) = self.load_images() {
            tracing::debug!("no exit hooks: listing images: {error}");
            return Vec::new();
        }
        let mut found = Vec::new();
        for (source, exit) in SOURCES.iter().enumerate() {
            let Some(load_address) = self
                .loaded_images
                .iter()
                .find(|image| image.descriptor_path().file_name().is_some_and(|name| name == exit.image))
                .map(|image| image.descriptor_load_address())
            else {
                tracing::debug!("no exit hooks: {} is not loaded", exit.image);
                continue;
            };
            if let Some(cached) = self
                .exit_images
                .iter()
                .find(|cached| cached.source == source && cached.load_address == load_address)
            {
                found.push(cached.clone());
                continue;
            }
            match self.read_exit_image(source, load_address) {
                Ok(image) => {
                    self.exit_images.retain(|cached| cached.source != source);
                    self.exit_images.push(image.clone());
                    found.push(image);
                }
                Err(detail) => tracing::debug!("no exit hooks in {}: {detail}", exit.image),
            }
        }
        found
    }

    fn read_exit_image(&mut self, source: usize, load_address: u64) -> std::result::Result<ExitImage, String>
    {
        let exit = &SOURCES[source];
        let header = self.read_mapped_header(load_address)?;
        let code = SymtabLayout::from_header(&header, load_address).map_err(|error| error.to_string())?;
        let table = super::stack::read_symbol_table(self.backend.as_mut(), &code)?;
        let mut entries = Vec::new();
        for (address, strx) in table.symbols().filter(|&(address, _)| code.contains(address)) {
            let name = super::stack::read_symbol_string(self.backend.as_mut(), &code, strx)?;
            if name.is_some_and(|name| (exit.entry)(&name)) && !entries.contains(&address) {
                entries.push(address);
            }
        }
        if entries.is_empty() {
            return Err(format!("{} has no exit entry symbol", exit.image));
        }
        Ok(ExitImage {
            source,
            load_address,
            code,
            entries,
        })
    }
}
