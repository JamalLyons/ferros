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

	.section	__DATA,__mod_init_func,mod_init_funcs
	.p2align	3
	.quad	_frameless_caller

	.subsections_via_symbols
