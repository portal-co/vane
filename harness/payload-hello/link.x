OUTPUT_ARCH(riscv)
ENTRY(_start)
SECTIONS
{
  /* vane loads payloads at the QEMU virt DRAM base, matching where the
     Linux kernel Image will sit in later milestones. */
  . = 0x80000000;
  .text : { *(.text._start) *(.text*) }
  .rodata : { *(.rodata*) }
  .data : { *(.data*) }
  .bss : { *(.bss*) *(COMMON) }
  /DISCARD/ : { *(.eh_frame*) *(.comment) }
}
