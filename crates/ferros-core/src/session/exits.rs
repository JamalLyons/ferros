//! Non-local exits: private traps on the code through which a thread leaves frames
//! without returning from them, so an operation that waits for a frame to return sees
//! the thread leave it by another route.
//!
//! The routes are the platform's: the session's
//! [`TargetRuntime::exit_sources`](crate::runtime::TargetRuntime::exit_sources) names
//! each image and its entry symbols (typically the system unwinder resuming at a landing
//! pad, for a panic caught further out, and the leaf of `longjmp`). A thread that hits
//! one is single-stepped until it leaves that image above the stack it entered with: it
//! has then landed where the exit goes. A session whose runtime names no source traps
//! nothing.

use std::sync::Arc;

use super::Session;
use super::images::same_image;
use super::stack::BackendTarget;
use crate::backend::StopEvent;
use crate::breakpoints::{BreakpointId, BreakpointKind, BreakpointRequest};
use crate::error::Result;
use crate::objfile::MappedSymbols;
use crate::runtime::ExitSource;
use crate::types::{Address, BuildIdentity, ThreadId};

/// Instructions single-stepped from an exit entry to its landing before giving up (an
/// unwinder resume takes a few dozen, a `longjmp` leaf a few tens).
const MAX_EXIT_STEPS: u32 = 4096;

/// One exit image's code and entries in one process.
#[derive(Clone)]
pub(super) struct ExitImage
{
    /// Index into the runtime's exit sources.
    source: usize,
    load_address: u64,
    build_id: BuildIdentity,
    /// Stop generation it was read at (see [`same_image`]).
    generation: u64,
    /// The image's code, which an exit is stepped through until it leaves.
    code: Arc<dyn MappedSymbols>,
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
    /// The traps outlived the continue they were armed for: a restore failed, and they
    /// are only waiting to be removed (#339). A pending handler return's stops are never
    /// matched against them; `retry_handler_cleanup` removes them before the next run.
    cleanup_pending: bool,
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
            cleanup_pending: false,
        }
    }

    pub(super) fn thread(&self) -> ThreadId
    {
        self.thread
    }

    /// Whether any trap is still armed.
    pub(super) fn armed(&self) -> bool
    {
        !self.hooks.is_empty()
    }

    /// Whether these traps only wait to be removed (see `cleanup_pending`).
    pub(super) fn cleanup_pending(&self) -> bool
    {
        self.cleanup_pending
    }

    /// Keep these traps only for their removal: a restore failed.
    pub(super) fn mark_cleanup_pending(&mut self)
    {
        self.cleanup_pending = true;
        self.leaving = None;
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
        // The user may have removed the breakpoint while cleanup was pending.
        let Some(address) = self.breakpoints.get(id).map(|entry| entry.info.address) else {
            return Ok(());
        };
        // Or set or enabled a logical breakpoint there while it was: the site still
        // trapped, so the logical breakpoint took it over (`insert_sites`), and the trap
        // is now the user's.
        if self
            .logical
            .iter()
            .any(|breakpoint| breakpoint.enabled && breakpoint.site_at(address).is_some())
        {
            return Ok(());
        }
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
            cleanup_pending: false,
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

    /// Attempt every hook restore, keeping only hooks whose restore failed. The first
    /// failure is returned and the option is cleared once all hooks are released.
    pub(super) fn restore_exit_hooks(&mut self, exits: &mut Option<ExitHooks>) -> Result<()>
    {
        let Some(armed) = exits else { return Ok(()) };
        let mut result = Ok(());
        armed.hooks.retain(|hook| {
            let Err(error) = self.release_site(hook.id, &hook.owner) else {
                return false;
            };
            if result.is_ok() {
                result = Err(error);
            }
            true
        });
        if armed.hooks.is_empty() {
            *exits = None;
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
    /// Cached while the source and load address match and the build can be the same (#170).
    ///
    /// A late build id that contradicts an opened image file (#221, #249) needs no refresh
    /// here. That contradiction is between the file on disk and the mapping, and nothing
    /// in an [`ExitImage`] comes from a file: `code` is the format's
    /// [`mapped_symbols`](crate::objfile::ObjectFormat::mapped_symbols) of the header
    /// mapped at `load_address`, and `entries` are read from that symbol table in the
    /// target's memory ([`read_exit_image`](Self::read_exit_image)), never through
    /// [`SymbolCache`](crate::symbols::SymbolCache). The id that completes an unknown
    /// `build_id` here describes that same mapping, which is what was read. A late id
    /// that differs from a known cached one fails [`same_image`] and the image is read
    /// again from memory. The ids come from [`load_images`](Self::load_images), which this
    /// calls first, so they are the session's current ones.
    fn exit_images(&mut self) -> Vec<ExitImage>
    {
        if let Err(error) = self.load_images() {
            tracing::debug!("no exit hooks: listing images: {error}");
            return Vec::new();
        }
        let mut previous = std::mem::take(&mut self.exit_images);
        let mut found = Vec::new();
        let runtime = Arc::clone(&self.runtime);
        for (source, exit) in runtime.exit_sources().iter().enumerate() {
            let Some(image) = self
                .loaded_images
                .iter()
                .find(|image| image.descriptor_path().file_name().is_some_and(|name| name == exit.image()))
            else {
                tracing::debug!("no exit hooks: {} is not loaded", exit.image());
                continue;
            };
            let load_address = image.descriptor_load_address();
            let build_id = image.build_id();
            if let Some(index) = previous.iter().position(|cached| {
                cached.source == source
                    && cached.load_address == load_address
                    && same_image(cached.build_id, cached.generation, build_id, self.stop_generation)
            }) {
                let mut cached = previous.swap_remove(index);
                cached.build_id = cached.build_id.or_later(build_id);
                found.push(cached);
                continue;
            }
            match self.read_exit_image(exit, source, load_address, build_id) {
                Ok(image) => found.push(image),
                Err(detail) => tracing::debug!("no exit hooks in {}: {detail}", exit.image()),
            }
        }
        self.exit_images.clone_from(&found);
        found
    }

    fn read_exit_image(
        &mut self,
        exit: &ExitSource,
        source: usize,
        load_address: u64,
        build_id: BuildIdentity,
    ) -> std::result::Result<ExitImage, String>
    {
        let mut memory = BackendTarget(self.backend.as_mut());
        let code = self.format.mapped_symbols(&mut memory, Address::new(load_address))?;
        let index = code.load(&mut memory)?;
        let mut entries = Vec::new();
        for (address, key) in index.symbols().filter(|&(address, _)| code.contains(address)) {
            let name = index.read_name(&mut memory, key)?;
            if name.is_some_and(|name| exit.is_entry(&name)) && !entries.contains(&address) {
                entries.push(address);
            }
        }
        if entries.is_empty() {
            return Err(format!("{} has no exit entry symbol", exit.image()));
        }
        Ok(ExitImage {
            source,
            load_address,
            build_id,
            generation: self.stop_generation,
            code,
            entries,
        })
    }
}
