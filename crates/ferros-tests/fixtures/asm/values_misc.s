// A function with no DWARF and a named interior instruction.
	.text
	.p2align	2
	.globl	_ferros_callback_entry
	.no_dead_strip	_ferros_callback_entry
_ferros_callback_entry:
	nop
	.globl	_ferros_callback_marker
	.alt_entry	_ferros_callback_marker
_ferros_callback_marker:
	ret
	.subsections_via_symbols
