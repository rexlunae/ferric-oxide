ENTRY(kstart)
OUTPUT_FORMAT(elf64-x86-64)

/* Ferric Oxide: the link address. KERNEL_OFFSET, which the kernel reads, is
 * defined in .dummy below relative to the image, so it stays right when the
 * bootloader moves the image (kernel image KASLR). */
KERNEL_LINK_BASE = 0xFFFFFFFF80000000;

SECTIONS {
    . = KERNEL_LINK_BASE;

    . += SIZEOF_HEADERS;

    /* Force the zero page to be part of a segment by creating a
     * dummy section in the zero page.
     * Limine will map the segment with the lowest vaddr value at
     * 0xFFFFFFFF80000000 even if the segment has a higher vaddr.
     * As such without the zero page being part of a segment, the
     * kernel would be loaded at an offset from the expected
     * location. As the redox kernel is not currently relocatable,
     * this would result in a crash. A similar issue likely exists
     * with multiboot/multiboot2 and the paddr of the segment.
     */
    .dummy : AT(ADDR(.dummy) - KERNEL_LINK_BASE) {
        KERNEL_OFFSET = . - SIZEOF_HEADERS;
    }

    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_LINK_BASE) {
        __text_start = .;
        *(.text*)
    }

    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_LINK_BASE) {
        __text_end = .;
        __rodata_start = .;
        *(.rodata*)
        __altcode_start = .;
        KEEP(*(.altcode*))
        __altcode_end = .;
        . = ALIGN(8);
        __altrelocs_start = .;
        KEEP(*(.altrelocs*))
        __altrelocs_end = .;
        __altfeatures_start = .;
        KEEP(*(.altfeatures*))
        __altfeatures_end = .;
        /* Ferric Oxide: tracepoint sites (src/arch/x86_64/static_key.rs). */
        . = ALIGN(8);
        __ferric_trace_keys_start = .;
        KEEP(*(.ferric_trace_keys*))
        __ferric_trace_keys_end = .;
    }

    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_LINK_BASE) {
        __rodata_end = .;
        *(.data*)
        . = ALIGN(4K);
        *(.bss*)
    }

    __end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.gcc_except_table*)
        *(.note*)
        *(.rel.eh_frame*)
    }
}
