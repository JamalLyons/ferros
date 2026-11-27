// Code for the `frameless_stack` fixture, assembled and linked by `build.rs` (arm64 macOS).
//
// `frameless_spill` is a leaf that allocates stack and has no frame record. Its CFI
// only says `CFA = sp + 32` after the `sub`, which the linker turns into the compact
// unwind encoding 0x02002000 (`UNWIND_ARM64_MODE_FRAMELESS`, stack size 32) with no
// `__eh_frame` FDE. That encoding is wrong between the `add sp` and the `ret`, where the
// caller's `sp` is the current `sp`, not `sp + 32`. Check it with
// `objdump --unwind-info` on the fixture binary. rustc gives Rust leaves like this DWARF
// CFI in a debug build, so the leaf is written in assembly.
//
// `frameless_caller` (compact unwind `UNWIND_ARM64_MODE_FRAME`) calls it and is run by
// dyld as an initializer through `__mod_init_func`, so the fixture calls the leaf
// without any `unsafe` Rust.
//
// `unwind_only_callee` saves the link register and the frame pointer in the reverse of
// the frame-record order, so the linker cannot encode it compactly and gives it
// `UNWIND_ARM64_MODE_DWARF`: only its `__eh_frame` FDE describes its caller. At the
// `nop` the frame pointer is zero and the link register points at
// the callee itself, so neither finds the caller `unwind_only_caller`. dyld runs
// `unwind_only_caller` as a second initializer.

	.text
	.globl	_frameless_spill
	.p2align	2
_frameless_spill:
	.cfi_startproc
	sub	sp, sp, #0x20
	.cfi_def_cfa_offset 32
	str	x0, [sp, #8]
	ldr	x0, [sp, #8]
	add	x0, x0, #1
	add	sp, sp, #0x20
	ret
	.cfi_endproc

	.globl	_frameless_caller
	.p2align	2
_frameless_caller:
	.cfi_startproc
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	mov	x0, #41
	bl	_frameless_spill
	ldp	x29, x30, [sp], #16
	ret
	.cfi_endproc

	.globl	_unwind_only_callee
	.p2align	2
_unwind_only_callee:
	.cfi_startproc
	stp	x30, x29, [sp, #-16]!
	.cfi_def_cfa_offset 16
	.cfi_offset w30, -16
	.cfi_offset w29, -8
	mov	x29, xzr
	adr	x30, _unwind_only_callee
	nop
	ldp	x30, x29, [sp], #16
	.cfi_def_cfa_offset 0
	.cfi_restore w30
	.cfi_restore w29
	ret
	.cfi_endproc

	.globl	_unwind_only_caller
	.p2align	2
_unwind_only_caller:
	.cfi_startproc
	stp	x29, x30, [sp, #-16]!
	mov	x29, sp
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	bl	_unwind_only_callee
	ldp	x29, x30, [sp], #16
	ret
	.cfi_endproc

	// Both functions use saved FP/LR in reverse order to require DWARF CFI.
	.globl	_frame_registers_callee
	.p2align	2
_frame_registers_callee:
	.cfi_startproc
	stp	x30, x29, [sp, #-32]!
	.cfi_def_cfa_offset 32
	.cfi_offset w30, -32
	.cfi_offset w29, -24
	stp	x19, x20, [sp, #16]
	.cfi_offset w19, -16
	.cfi_offset w20, -8
	mov	x19, #0x5151
	mov	x20, #0x6161
	mov	x0, #0x7171
	nop
	ldp	x19, x20, [sp, #16]
	ldp	x30, x29, [sp], #32
	.cfi_def_cfa_offset 0
	.cfi_restore w19
	.cfi_restore w20
	.cfi_restore w29
	.cfi_restore w30
	ret
	.cfi_endproc

	.globl	_frame_registers_caller
	.p2align	2
_frame_registers_caller:
	.cfi_startproc
	stp	x30, x29, [sp, #-48]!
	.cfi_def_cfa_offset 48
	.cfi_offset w30, -48
	.cfi_offset w29, -40
	stp	x19, x20, [sp, #16]
	.cfi_offset w19, -32
	.cfi_offset w20, -24
	mov	x29, sp
	mov	x19, #0x3131
	mov	x20, xzr
	mov	x0, #0x4141
	bl	_frame_registers_callee
	ldp	x19, x20, [sp, #16]
	ldp	x30, x29, [sp], #48
	.cfi_def_cfa_offset 0
	.cfi_restore w19
	.cfi_restore w20
	.cfi_restore w29
	.cfi_restore w30
	ret
	.cfi_endproc

	.section	__DATA,__mod_init_func,mod_init_funcs
	.p2align	3
	.quad	_frameless_caller
	.quad	_unwind_only_caller
	.quad	_frame_registers_caller

	.subsections_via_symbols
