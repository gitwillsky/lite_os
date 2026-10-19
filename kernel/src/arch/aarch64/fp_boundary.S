    .arch armv8-a+simd
    .section .text
    .align 4

    .macro OPEN_FP
        mrs x11, cpacr_el1
        orr x11, x11, #(3 << 20)
        msr cpacr_el1, x11
        isb
    .endm

    .macro CLOSE_FP
        mrs x11, cpacr_el1
        bic x11, x11, #(3 << 20)
        msr cpacr_el1, x11
        isb
    .endm

    // x0 points at fpsr/fpcr/pad/vregs[32] inside one aligned FPSIMD record.
    .global __aarch64_signal_fp_capture
__aarch64_signal_fp_capture:
    OPEN_FP
    mrs x9, fpsr
    mrs x10, fpcr
    str w9, [x0, #0]
    str w10, [x0, #4]
    add x12, x0, #8
    stp q0, q1, [x12, #0]
    stp q2, q3, [x12, #32]
    stp q4, q5, [x12, #64]
    stp q6, q7, [x12, #96]
    stp q8, q9, [x12, #128]
    stp q10, q11, [x12, #160]
    stp q12, q13, [x12, #192]
    stp q14, q15, [x12, #224]
    stp q16, q17, [x12, #256]
    stp q18, q19, [x12, #288]
    stp q20, q21, [x12, #320]
    stp q22, q23, [x12, #352]
    stp q24, q25, [x12, #384]
    stp q26, q27, [x12, #416]
    stp q28, q29, [x12, #448]
    stp q30, q31, [x12, #480]
    CLOSE_FP
    ret

    // x0 points at a validated immutable FPSIMD record body.
    .global __aarch64_signal_fp_restore
__aarch64_signal_fp_restore:
    OPEN_FP
    ldr w9, [x0, #0]
    ldr w10, [x0, #4]
    msr fpsr, x9
    msr fpcr, x10
    add x12, x0, #8
    ldp q0, q1, [x12, #0]
    ldp q2, q3, [x12, #32]
    ldp q4, q5, [x12, #64]
    ldp q6, q7, [x12, #96]
    ldp q8, q9, [x12, #128]
    ldp q10, q11, [x12, #160]
    ldp q12, q13, [x12, #192]
    ldp q14, q15, [x12, #224]
    ldp q16, q17, [x12, #256]
    ldp q18, q19, [x12, #288]
    ldp q20, q21, [x12, #320]
    ldp q22, q23, [x12, #352]
    ldp q24, q25, [x12, #384]
    ldp q26, q27, [x12, #416]
    ldp q28, q29, [x12, #448]
    ldp q30, q31, [x12, #480]
    CLOSE_FP
    ret

    // x0 points at an unpublished KernelContext; vector fields begin at byte 112.
    .global __aarch64_clone_fp_capture
__aarch64_clone_fp_capture:
    OPEN_FP
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
    str x9, [x0, #624]
    str x10, [x0, #632]
    CLOSE_FP
    ret

    // x0 points at an aligned zero fpsr/fpcr/pad/vregs image.
    .global __aarch64_exec_fp_reset
__aarch64_exec_fp_reset:
    OPEN_FP
    msr fpsr, xzr
    msr fpcr, xzr
    add x12, x0, #8
    ldp q0, q1, [x12, #0]
    ldp q2, q3, [x12, #32]
    ldp q4, q5, [x12, #64]
    ldp q6, q7, [x12, #96]
    ldp q8, q9, [x12, #128]
    ldp q10, q11, [x12, #160]
    ldp q12, q13, [x12, #192]
    ldp q14, q15, [x12, #224]
    ldp q16, q17, [x12, #256]
    ldp q18, q19, [x12, #288]
    ldp q20, q21, [x12, #320]
    ldp q22, q23, [x12, #352]
    ldp q24, q25, [x12, #384]
    ldp q26, q27, [x12, #416]
    ldp q28, q29, [x12, #448]
    ldp q30, q31, [x12, #480]
    CLOSE_FP
    ret
