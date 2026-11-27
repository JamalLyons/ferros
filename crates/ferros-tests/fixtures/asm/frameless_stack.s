// Code for the `frameless_stack` fixture, assembled and linked by `build.rs` (arm64 macOS).
//
// `frameless_spill` is a leaf that allocates stack and has no frame record. Its CFI
// describes a 32-byte allocation and the x19/x20 saves. The linker turns it into
// 0x02002001 (`UNWIND_ARM64_MODE_FRAMELESS`, stack size 32, x19/x20 saved) with no
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
	// Conditional branches put the pre-save stop outside both instruction analyses.
	cbz	x0, 1f
1:	mov	x9, x9
	cbz	x0, 2f
2:
	stp	x20, x19, [sp, #16]
	.cfi_offset w19, -8
	.cfi_offset w20, -16
	mov	x19, #0x5151
	str	x0, [sp, #8]
	ldr	x0, [sp, #8]
	add	x0, x0, #1
	cbz	x0, 1f
1:	nop
	cbz	x0, 2f
2:	ldp	x20, x19, [sp, #16]
	add	sp, sp, #0x20
	ret
	.cfi_endproc

	.globl	_frameless_locals
	.p2align	2
_frameless_locals:
	.cfi_startproc
	sub	sp, sp, #16
	.cfi_def_cfa_offset 16
	cbz	x0, 1f
1:	nop
	cbz	x0, 2f
2:	add	sp, sp, #16
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
	bl	_frameless_locals
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
	bl	_compact_registers_callee
	ldp	x19, x20, [sp, #16]
	ldp	x30, x29, [sp], #48
	.cfi_def_cfa_offset 0
	.cfi_restore w19
	.cfi_restore w20
	.cfi_restore w29
	.cfi_restore w30
	ret
	.cfi_endproc

	// Standard frame record with a saved x19/x20 pair. Stop in its child to
	// recover these slots from the hint for a caller frame.
	.globl	_compact_registers_callee
	.p2align	2
_compact_registers_callee:
	.cfi_startproc
	sub	sp, sp, #32
	stp	x29, x30, [sp, #16]
	add	x29, sp, #16
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	stp	x20, x19, [sp]
	.cfi_offset w19, -24
	.cfi_offset w20, -32
	mov	x19, #0x5252
	mov	x20, #0x6262
	bl	_compact_unbuilt_frame
	ldp	x20, x19, [sp]
	ldp	x29, x30, [sp, #16]
	add	sp, sp, #32
	ret
	.cfi_endproc

	// The branches around the nop defeat both instruction decoders before FP is
	// installed. The live FP still points to _compact_registers_callee's frame.
	.globl	_compact_unbuilt_frame
	.p2align	2
_compact_unbuilt_frame:
	.cfi_startproc
	cbz	x0, 3f
3:	nop
	cbz	x0, 4f
4:	sub	sp, sp, #48
	stp	x29, x30, [sp, #32]
	add	x29, sp, #32
	.cfi_def_cfa w29, 16
	.cfi_offset w30, -8
	.cfi_offset w29, -16
	stp	x20, x19, [sp, #16]
	.cfi_offset w19, -24
	.cfi_offset w20, -32
	stp	x22, x21, [sp]
	.cfi_offset w21, -40
	.cfi_offset w22, -48
	ldp	x22, x21, [sp]
	ldp	x20, x19, [sp, #16]
	ldp	x29, x30, [sp, #32]
	add	sp, sp, #48
	ret
	.cfi_endproc

	.section	__DATA,__mod_init_func,mod_init_funcs
	.p2align	3
	.quad	_frameless_caller
	.quad	_unwind_only_caller
	.quad	_frame_registers_caller

	.subsections_via_symbols
