SECTIONS {
 .preinit_array : { __preinit_array_start = .; KEEP(*(.preinit_array)) __preinit_array_end = .; }
 .init_array : { __init_array_start = .; KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*))) KEEP(*(.init_array)) __init_array_end = .; }
 .fini_array : { __fini_array_start = .; KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*))) KEEP(*(.fini_array)) __fini_array_end = .; }
} INSERT BEFORE .dynamic;
