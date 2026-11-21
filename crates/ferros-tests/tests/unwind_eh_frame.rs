//! `__eh_frame` FDE ranges are runtime addresses; the unwinder must not match them against
//! the unslid file address.
//!
//! The image is a synthetic arm64 Mach-O written to a temporary file: no `__unwind_info`
//! (so the unwinder takes the linear `__eh_frame` scan) and an `__eh_frame` with two FDEs
//! with different CFA rules. The load address is slid by less than the text size, so the
//! file address of a pc inside function A's runtime range falls inside function B's runtime
//! range. Unwinding from that pc must use A's rule.
//!
//! A second image adds `__eh_frame_hdr` and hides the FDEs from the linear scan behind
//! an early terminator, so only the header's binary-search table can find them. That pins
//! `unwind_with_eh_frame_hdr` to the runtime lookup address and to a table based at the
//! slid `__eh_frame_hdr` address.
//!
//! Like `backend_mock.rs`, this launches no process: it needs no `harness::run`, no fixture
//! and no debugger entitlement, and is not limited to macOS.

use std::collections::HashMap;
use std::path::PathBuf;

use ferros_core::arch;
use ferros_core::backend::mock::arm64_registers;
use ferros_core::error::DebuggerError;
use ferros_core::symbols::unwind::{MemoryAccess, StackUnwinder};
use ferros_core::symbols::{DebugInfoOptions, ImageDescriptor, SymbolCache};
use ferros_core::types::{Address, Architecture, Registers, StackFrame, ThreadId};
use ferros_tests::scratch;

/// Link-time address of the `__TEXT` segment.
const FILE_BASE: u64 = 0x1_0000_0000;
/// ASLR slide: page-aligned and smaller than the text size.
const SLIDE: u64 = 0x4000;
const LOAD_ADDRESS: u64 = FILE_BASE + SLIDE;
/// `vmsize` of `__TEXT`, larger than the slide.
const TEXT_SIZE: u64 = 0x1_0000;

/// File (link-time) addresses of the two functions. `FUNCTION_A` is exactly one slide
/// above `FUNCTION_B`, so B's runtime range starts at `FUNCTION_A`: a pc at
/// `FUNCTION_A + SLIDE + 0x10` (inside A's runtime range) has the file address
/// `FUNCTION_A + 0x10`, which is inside B's runtime range.
const FUNCTION_B: u64 = FILE_BASE + 0x1000;
const FUNCTION_A: u64 = FUNCTION_B + SLIDE;
const FUNCTION_SIZE: u32 = 0x100;

/// Where each section lives in the file, which is also its offset from `FILE_BASE`.
const EH_FRAME_HDR_OFFSET: u64 = 0x300;
const EH_FRAME_OFFSET: u64 = 0x400;
const DEBUG_OFFSET: u64 = 0x500;
const FILE_SIZE: u64 = 0x600;

/// Return addresses the two CFA rules recover; both outside the image, so the trace ends
/// after the caller frame.
const CALLER_OF_A: u64 = 0x2_0000_000a;
const CALLER_OF_B: u64 = 0x2_0000_0b0b;
const STACK_POINTER: u64 = 0x7000;

fn push_u32(bytes: &mut Vec<u8>, value: u32)
{
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64)
{
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_name(bytes: &mut Vec<u8>, name: &str)
{
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name.as_bytes());
    bytes.extend_from_slice(&field);
}

/// One `section_64` load-command entry inside `__TEXT`.
fn push_section(bytes: &mut Vec<u8>, name: &str, address: u64, size: u64, offset: u64)
{
    push_name(bytes, name);
    push_name(bytes, "__TEXT");
    push_u64(bytes, address);
    push_u64(bytes, size);
    push_u32(bytes, u32::try_from(offset).expect("offset"));
    push_u32(bytes, 0); // align
    push_u32(bytes, 0); // reloff
    push_u32(bytes, 0); // nreloc
    push_u32(bytes, 0); // flags
    push_u32(bytes, 0); // reserved1
    push_u32(bytes, 0); // reserved2
    push_u32(bytes, 0); // reserved3
}

/// A CIE with the augmentation `zR`, FDE pointers encoded `DW_EH_PE_pcrel | sdata4`.
/// 24 bytes.
fn cie() -> Vec<u8>
{
    let mut bytes = Vec::new();
    push_u32(&mut bytes, 20); // length
    push_u32(&mut bytes, 0); // CIE id
    bytes.push(1); // version
    bytes.extend_from_slice(b"zR\0");
    bytes.push(4); // code alignment
    bytes.push(0x78); // data alignment -8
    bytes.push(30); // return address register
    bytes.push(1); // augmentation data length
    bytes.push(0x1b); // FDE pointer encoding
    bytes.extend_from_slice(&[0x0c, 31, 0]); // DW_CFA_def_cfa sp, 0
    bytes.extend_from_slice(&[0; 4]); // DW_CFA_nop padding
    bytes
}

/// A zero-length entry: the end of `__eh_frame` for any scan that walks it.
fn terminator() -> Vec<u8>
{
    vec![0; 4]
}

/// An FDE for `[function, function + FUNCTION_SIZE)` at `offset` in the section whose
/// CFA is `sp + cfa_offset` with the return address saved at `cfa - 8`. 24 bytes.
fn fde(offset: u64, function: u64, cfa_offset: u8) -> Vec<u8>
{
    let section_address = FILE_BASE + EH_FRAME_OFFSET;
    let pc_begin_field = section_address + offset + 8;
    let pc_begin = i32::try_from(i128::from(function) - i128::from(pc_begin_field)).expect("pc-relative");
    let mut bytes = Vec::new();
    push_u32(&mut bytes, 20); // length
    push_u32(&mut bytes, u32::try_from(offset + 4).expect("cie pointer")); // back to the CIE at 0
    bytes.extend_from_slice(&pc_begin.to_le_bytes());
    push_u32(&mut bytes, FUNCTION_SIZE);
    bytes.push(0); // augmentation data length
    bytes.extend_from_slice(&[0x0c, 31, cfa_offset]); // DW_CFA_def_cfa sp, cfa_offset
    bytes.extend_from_slice(&[0x9e, 1]); // DW_CFA_offset (0x80 | 30): x30 at cfa - 8
    bytes.extend_from_slice(&[0; 2]);
    bytes
}

/// Which unwind sections the synthetic image carries.
#[derive(Clone, Copy, Debug)]
enum Layout
{
    /// `__eh_frame` only, every FDE reachable by the linear scan.
    EhFrame,
    /// `__eh_frame_hdr` plus an `__eh_frame` whose FDEs follow a terminator, so only the
    /// header table reaches them.
    EhFrameHdr,
}

/// `.eh_frame_hdr` (file address `hdr`) indexing FDEs at file addresses `fdes`, as
/// `(function, fde)` pairs sorted by function. Every pointer is relative to an address in
/// the image, so the table is correct only when based at the slid header address.
fn eh_frame_hdr(fdes: &[(u64, u64)]) -> Vec<u8>
{
    let hdr = FILE_BASE + EH_FRAME_HDR_OFFSET;
    let relative = |to: u64, from: u64| i32::try_from(i128::from(to) - i128::from(from)).expect("relative");
    let mut bytes = vec![
        1,    // version
        0x1b, // eh_frame_ptr: DW_EH_PE_pcrel | sdata4
        0x03, // fde_count: DW_EH_PE_udata4
        0x3b, // table: DW_EH_PE_datarel | sdata4
    ];
    bytes.extend_from_slice(&relative(FILE_BASE + EH_FRAME_OFFSET, hdr + 4).to_le_bytes());
    push_u32(&mut bytes, u32::try_from(fdes.len()).expect("count"));
    for &(function, fde) in fdes {
        bytes.extend_from_slice(&relative(function, hdr).to_le_bytes());
        bytes.extend_from_slice(&relative(fde, hdr).to_le_bytes());
    }
    bytes
}

/// The Mach-O file: header, one `__TEXT` segment holding `__eh_frame` (and, for
/// [`Layout::EhFrameHdr`], `__eh_frame_hdr`) and placeholder DWARF sections (so the image
/// needs no dSYM), and no `__unwind_info`.
fn mach_o(layout: Layout) -> Vec<u8>
{
    // Function B is first in the section, so a scan that wrongly matched the file address
    // would find B before A.
    let mut eh_frame = cie();
    let mut hdr = None;
    match layout {
        Layout::EhFrame => {
            eh_frame.extend(fde(24, FUNCTION_B, 32));
            eh_frame.extend(fde(48, FUNCTION_A, 16));
        }
        Layout::EhFrameHdr => {
            eh_frame.extend(terminator());
            eh_frame.extend(fde(28, FUNCTION_B, 32));
            eh_frame.extend(fde(52, FUNCTION_A, 16));
            let section = FILE_BASE + EH_FRAME_OFFSET;
            hdr = Some(eh_frame_hdr(&[(FUNCTION_B, section + 28), (FUNCTION_A, section + 52)]));
        }
    }
    eh_frame.extend(terminator());

    let mut sections = vec![("__eh_frame", EH_FRAME_OFFSET, u64::try_from(eh_frame.len()).expect("len"))];
    if let Some(hdr) = &hdr {
        sections.push(("__eh_frame_hdr", EH_FRAME_HDR_OFFSET, u64::try_from(hdr.len()).expect("len")));
    }
    sections.extend(vec![
        ("__debug_info", DEBUG_OFFSET, 8),
        ("__debug_abbrev", DEBUG_OFFSET + 8, 8),
        ("__debug_line", DEBUG_OFFSET + 16, 8),
    ]);
    let command_size = 72 + 80 * sections.len();

    let mut bytes = Vec::new();
    push_u32(&mut bytes, 0xfeed_facf); // MH_MAGIC_64
    push_u32(&mut bytes, 0x0100_000c); // CPU_TYPE_ARM64
    push_u32(&mut bytes, 0); // cpusubtype
    push_u32(&mut bytes, 2); // MH_EXECUTE
    push_u32(&mut bytes, 1); // ncmds
    push_u32(&mut bytes, u32::try_from(command_size).expect("size"));
    push_u32(&mut bytes, 0); // flags
    push_u32(&mut bytes, 0); // reserved

    push_u32(&mut bytes, 0x19); // LC_SEGMENT_64
    push_u32(&mut bytes, u32::try_from(command_size).expect("size"));
    push_name(&mut bytes, "__TEXT");
    push_u64(&mut bytes, FILE_BASE);
    push_u64(&mut bytes, TEXT_SIZE);
    push_u64(&mut bytes, 0); // fileoff: the header is part of the segment
    push_u64(&mut bytes, FILE_SIZE);
    push_u32(&mut bytes, 5); // maxprot
    push_u32(&mut bytes, 5); // initprot
    push_u32(&mut bytes, u32::try_from(sections.len()).expect("count"));
    push_u32(&mut bytes, 0); // flags
    for (name, offset, size) in sections {
        push_section(&mut bytes, name, FILE_BASE + offset, size, offset);
    }

    if let Some(hdr) = &hdr {
        bytes.resize(usize::try_from(EH_FRAME_HDR_OFFSET).expect("offset"), 0);
        bytes.extend_from_slice(hdr);
    }
    bytes.resize(usize::try_from(EH_FRAME_OFFSET).expect("offset"), 0);
    bytes.extend_from_slice(&eh_frame);
    bytes.resize(usize::try_from(DEBUG_OFFSET).expect("offset"), 0);
    bytes.extend_from_slice(&[0xaa; 24]); // placeholder DWARF, never parsed by the unwinder
    bytes.resize(usize::try_from(FILE_SIZE).expect("size"), 0);
    bytes
}

/// The image on disk for the duration of a test.
struct TempImage(PathBuf);

impl TempImage
{
    fn create(name: &str, layout: Layout) -> Self
    {
        let (mut file, path) = scratch::create_file(&format!("ferros-{name}"), ".macho");
        std::io::Write::write_all(&mut file, &mach_o(layout)).expect("write synthetic image");
        Self(path)
    }
}

impl Drop for TempImage
{
    fn drop(&mut self)
    {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A stack of little-endian words; anything else is unreadable.
struct Stack(HashMap<u64, u64>);

impl MemoryAccess for Stack
{
    fn read_u64(&self, address: Address) -> ferros_core::Result<u64>
    {
        self.0
            .get(&address.value())
            .copied()
            .ok_or_else(|| DebuggerError::InvalidArgument(format!("unreadable {address}")))
    }
}

/// Load the synthetic image slid to `LOAD_ADDRESS` and unwind from `pc`.
fn unwind_from(layout: Layout, pc: u64) -> Vec<StackFrame>
{
    let image = TempImage::create(&format!("eh-frame-{layout:?}-{pc:x}"), layout);
    let mut symbols = SymbolCache::with_debug_info_options(DebugInfoOptions {
        dsymutil: None,
        ..DebugInfoOptions::default()
    });
    let loaded = symbols
        .load_image(ImageDescriptor {
            path: image.0.clone(),
            load_address: LOAD_ADDRESS,
        })
        .expect("load synthetic image");
    assert_eq!(loaded.architecture(), Architecture::Arm64);
    assert!(loaded.contains(Address::new(pc)), "pc is inside the slid image");

    // A's rule reads the return address at 0x7008, B's at 0x7018.
    let memory = Stack(HashMap::from([
        (STACK_POINTER + 8, CALLER_OF_A),
        (STACK_POINTER + 24, CALLER_OF_B),
    ]));
    let arch = arch::for_architecture(Architecture::Arm64).expect("arm64");
    let mut regs: Registers = arm64_registers();
    regs.pc = Address::new(pc);
    regs.sp = Address::new(STACK_POINTER);
    arch.set_fp(&mut regs, Address::ZERO);
    arch.set_link_register(&mut regs, Address::new(0x2_dead_0000)).expect("lr");

    StackUnwinder::new(arch, &symbols, &memory)
        .unwind(ThreadId::from(1), &regs, 8)
        .expect("unwind")
}

#[test]
fn eh_frame_fde_is_matched_by_runtime_address_not_file_address()
{
    // Runtime pc in A's runtime range; its file address (pc - slide) lies in B's
    // *runtime* range, which is what a file-address match against `__eh_frame` would hit.
    let pc = FUNCTION_A + SLIDE + 0x10;
    let file_address = pc - SLIDE;
    assert!((FUNCTION_B + SLIDE..FUNCTION_B + SLIDE + u64::from(FUNCTION_SIZE)).contains(&file_address));

    let frames = unwind_from(Layout::EhFrame, pc);
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(
        pcs,
        [pc, CALLER_OF_A],
        "A's rule (CFA = sp + 16, return address at cfa - 8) must be used, not B's"
    );
    assert_eq!(frames[1].sp, Address::new(STACK_POINTER + 16), "caller sp is A's CFA");
}

#[test]
fn eh_frame_fde_for_the_other_function_still_matches_by_runtime_address()
{
    // Control: the same image, a pc in B's runtime range, picks B's rule.
    let pc = FUNCTION_B + SLIDE + 0x10;
    let frames = unwind_from(Layout::EhFrame, pc);
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(pcs, [pc, CALLER_OF_B]);
    assert_eq!(frames[1].sp, Address::new(STACK_POINTER + 32), "caller sp is B's CFA");
}

#[test]
fn eh_frame_hdr_table_is_searched_by_runtime_address_from_the_slid_header()
{
    // Only the header table can reach the FDEs.
    //
    // A pc in A's runtime range: looking up its file address would land on B's entry
    // (as in the linear-scan test above).
    let pc_in_a = FUNCTION_A + SLIDE + 0x10;
    let frames = unwind_from(Layout::EhFrameHdr, pc_in_a);
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(
        pcs,
        [pc_in_a, CALLER_OF_A],
        "the header table must find A's FDE (CFA = sp + 16, return address at cfa - 8)"
    );
    assert_eq!(frames[1].sp, Address::new(STACK_POINTER + 16), "caller sp is A's CFA");

    // A pc in B's runtime range: basing the table at the unslid header address shifts
    // every search key down by the slide, so A's key (`FUNCTION_A`, B's runtime start)
    // would be picked, and A's FDE does not cover this pc.
    let pc_in_b = FUNCTION_B + SLIDE + 0x10;
    let frames = unwind_from(Layout::EhFrameHdr, pc_in_b);
    let pcs: Vec<u64> = frames.iter().map(|frame| frame.pc.value()).collect();
    assert_eq!(pcs, [pc_in_b, CALLER_OF_B], "the header table must find B's FDE");
    assert_eq!(frames[1].sp, Address::new(STACK_POINTER + 32), "caller sp is B's CFA");
}
