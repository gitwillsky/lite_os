    .arch armv8-a+simd
    .section .text
    .align 4
    .global __switch
__switch:
    mrs x9, cpacr_el1
    orr x9, x9, #(3 << 20)
    msr cpacr_el1, x9
    isb
    stp x19, x20, [x0, #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]
    mov x9, sp
    str x9, [x0, #96]
    stp q0, q1, [x0, #112]
    stp q2, q3, [x0, #144]
    stp q4, q5, [x0, #176]
    stp q6, q7, [x0, #208]
    stp q8, q9, [x0, #240]
    stp q10, q11, [x0, #272]
    stp q12, q13, [x0, #304]
    stp q14, q15, [x0, #336]
    stp q16, q17, [x0, #368]
    stp q18, q19, [x0, #400]
    stp q20, q21, [x0, #432]
    stp q22, q23, [x0, #464]
    stp q24, q25, [x0, #496]
    stp q26, q27, [x0, #528]
    stp q28, q29, [x0, #560]
    stp q30, q31, [x0, #592]
    mrs x9, fpcr
    mrs x10, fpsr
    add x11, x0, #624
    stp x9, x10, [x11]

    add x11, x1, #624
    ldp x9, x10, [x11]
    msr fpcr, x9
    msr fpsr, x10
    ldp q0, q1, [x1, #112]
    ldp q2, q3, [x1, #144]
    ldp q4, q5, [x1, #176]
    ldp q6, q7, [x1, #208]
    ldp q8, q9, [x1, #240]
    ldp q10, q11, [x1, #272]
    ldp q12, q13, [x1, #304]
    ldp q14, q15, [x1, #336]
    ldp q16, q17, [x1, #368]
    ldp q18, q19, [x1, #400]
    ldp q20, q21, [x1, #432]
    ldp q22, q23, [x1, #464]
    ldp q24, q25, [x1, #496]
    ldp q26, q27, [x1, #528]
    ldp q28, q29, [x1, #560]
    ldp q30, q31, [x1, #592]
    ldp x19, x20, [x1, #0]
    ldp x21, x22, [x1, #16]
    ldp x23, x24, [x1, #32]
    ldp x25, x26, [x1, #48]
    ldp x27, x28, [x1, #64]
    ldp x29, x30, [x1, #80]
    ldr x9, [x1, #96]
    mov sp, x9
    mrs x9, cpacr_el1
    bic x9, x9, #(3 << 20)
    msr cpacr_el1, x9
    isb
    ret
