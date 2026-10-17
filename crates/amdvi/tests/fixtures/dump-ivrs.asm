; Fixture tool for ferric-amdvi: dumps the ACPI IVRS table, and the first AMD-Vi unit's MMIO
; registers 0x00-0x47 (device table, command buffer and event log bases, control, exclusion
; range, extended features), on COM1, then powers the machine off. It is a Multiboot v1 flat
; binary for QEMU's -kernel option, running in 32-bit protected mode without paging, and uses
; SeaBIOS's RSDP in 0xE0000-0xFFFFF. It is ferric-vtd's dump-dmar.asm looking for "IVRS".
;
; The .ivrs fixtures next to this file came from QEMU 8.2.2 (the ubuntu-24.04 package, as on the
; CI runners) and QEMU 10.2.1 (the ubuntu-26.04 package, which CI's AMD-Vi boots use):
;
;   nasm -f bin -o dump-ivrs.bin dump-ivrs.asm
;   qemu-system-x86_64 -machine q35,kernel-irqchip=split -device amd-iommu,intremap=off \
;       -m 2048 -smp 4 -vga none -drive file=d.img,format=raw,if=none,id=drv0 \
;       -device nvme,drive=drv0,serial=NVME_SERIAL,addr=4.0 -netdev user,id=net0 \
;       -device e1000,netdev=net0,addr=5.0 -device edu,addr=10.0 -kernel dump-ivrs.bin \
;       -nographic -no-reboot -serial stdio -monitor none
;   -> qemu-8.2-q35-intremap-off.ivrs  (8.2: one IVHD of type 10h, select entries)
;   ... -device amd-iommu,intremap=on
;   -> qemu-8.2-q35-intremap-on.ivrs   (adds a special entry for the I/O APIC)
;   QEMU 10.2 ... -device amd-iommu,intremap=off,dma-remap=on
;   -> qemu-10.2-q35-dma-remap.ivrs    (IVHDs of types 10h and 11h for the same unit)
;   QEMU 10.2 ... -device amd-iommu,intremap=on,xtsup=on,dma-remap=on
;   -> qemu-10.2-q35-intremap-xt.ivrs
;
; The hex after "IVRS:" is the table; the EFR values in src/regs.rs come from the "REGS" line
; (the word at 0x30: 0x29d3, or 0x29d7 with xtsup=on).
; Multiboot v1 flat binary: find the ACPI IVRS table and print it in hex on COM1.
bits 32
org 0x100000
MB_MAGIC equ 0x1BADB002
MB_FLAGS equ 0x00010000
header:
    dd MB_MAGIC, MB_FLAGS, -(MB_MAGIC + MB_FLAGS)
    dd header, header, end, end, start
start:
    mov esp, 0x90000
    ; RSDP lives on a 16-byte boundary in 0xE0000-0xFFFFF
    mov esi, 0xE0000
.scan:
    cmp dword [esi], 'RSD '
    jne .next
    cmp dword [esi+4], 'PTR '
    je .found
.next:
    add esi, 16
    cmp esi, 0x100000
    jb .scan
    mov esi, msg_nordsp
    call puts
    jmp halt
.found:
    mov ebx, [esi+16]          ; RSDT
    mov ecx, [ebx+4]
    sub ecx, 36
    shr ecx, 2
    lea edi, [ebx+36]
.walk:
    test ecx, ecx
    jz .none
    mov eax, [edi]
    cmp dword [eax], 'IVRS'
    je .dump
    add edi, 4
    dec ecx
    jmp .walk
.none:
    mov esi, msg_nodmar
    call puts
    jmp halt
.dump:
    push eax
    mov esi, eax
    mov ecx, [esi+4]
    push esi
    mov esi, msg_begin
    call puts
    pop esi
.byte:
    mov al, [esi]
    call puthex
    inc esi
    dec ecx
    jnz .byte
    mov esi, msg_end
    call puts
    ; IOMMU register base from the first IVHD (offset 48+8)
    pop eax
    mov ebx, [eax+56]
    mov esi, msg_regs
    call puts
    mov edi, 0
.reg:
    mov edx, [ebx+edi+4]
    call put32
    mov edx, [ebx+edi]
    call put32
    mov al, ' '
    call putc
    add edi, 8
    cmp edi, 0x48
    jb .reg
    mov al, 10
    call putc
halt:
    mov dx, 0x604               ; q35 ACPI PM1a control: S5 poweroff
    mov ax, 0x2000
    out dx, ax
    cli
    hlt
    jmp halt
putc:                           ; al
    push edx
    push eax
    mov dx, 0x3fd
.w: in al, dx
    test al, 0x20
    jz .w
    pop eax
    mov dx, 0x3f8
    out dx, al
    pop edx
    ret
puthex:
    push eax
    shr al, 4
    call nib
    pop eax
    and al, 0xf
nib:
    and al, 0xf
    add al, '0'
    cmp al, '9'
    jbe .o
    add al, 7
.o: jmp putc
put32:
    mov ecx, 4
.p: rol edx, 8
    mov al, dl
    call puthex
    dec ecx
    jnz .p
    ret
puts:
    lodsb
    test al, al
    jz .d
    call putc
    jmp puts
.d: ret
msg_begin: db 10, 'IVRS:', 0
msg_end: db 10, 'END', 10, 0
msg_regs: db 'REGS(u64 at 0x00..0x38):', 0
msg_nordsp: db 'no RSDP', 10, 0
msg_nodmar: db 'no IVRS', 10, 0
end:
