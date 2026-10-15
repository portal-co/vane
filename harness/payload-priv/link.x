OUTPUT_ARCH(riscv)
ENTRY(_start)
SECTIONS
{
  /* vane loads payloads at the QEMU virt DRAM base. */
  . = 0x80000000;
  .text : { *(.text._start) *(.text*) }
  .rodata : { *(.rodata*) }
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) *(.sbss*) }
  /* riscv-tests-style host communication word at a fixed address so
     vane-cli needs no symbol table. */
  . = 0x80010000;
  .tohost : { *(.tohost) }
  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
