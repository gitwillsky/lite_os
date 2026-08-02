# Make 是稳定的用户入口；workflow.py 拥有依赖顺序、缓存与 QEMU argv。
ARCH ?= aarch64
ACCEL ?= hvf
PROFILE ?= release
PYTHON ?= python3

ifneq ($(filter $(ARCH),aarch64 riscv64),$(ARCH))
$(error ARCH must be one of: aarch64, riscv64; got '$(ARCH)')
endif
ifneq ($(filter $(ACCEL),hvf tcg),$(ACCEL))
$(error ACCEL must be one of: hvf, tcg; got '$(ACCEL)')
endif
ifeq ($(ARCH)-$(ACCEL),riscv64-hvf)
$(error ACCEL=hvf is not supported for ARCH=riscv64; use ACCEL=tcg)
endif
ifneq ($(filter $(PROFILE),release debug),$(PROFILE))
$(error PROFILE must be one of: release, debug; got '$(PROFILE)')
endif

# build_target.py 是 ARCH/ACCEL 的唯一映射 owner；Make 解析期验证组合，避免 workflow
# 启动后才发现 kernel、rootfs 与 QEMU 选中了不同 backend。
TARGET_VALIDATION := $(shell ARCH=$(ARCH) ACCEL=$(ACCEL) $(PYTHON) scripts/build_target.py --field TARGET_KEY >/dev/null 2>&1 && echo ok)
ifneq ($(TARGET_VALIDATION),ok)
$(error ARCH/ACCEL combination is not supported: ARCH=$(ARCH) ACCEL=$(ACCEL))
endif

FS_IMAGE_SIZE_MIB ?= 8192
AGENT_FS_IMAGE_SIZE_MIB ?= 32768
QEMU_MEMORY ?= 2G
AGENT_QEMU_MEMORY ?= 6G
QEMU_GUI_DISPLAY ?= cocoa,zoom-to-fit=off
QEMU_GUI_WINDOW_WIDTH ?= 1504
QEMU_GUI_WINDOW_HEIGHT ?= 874
QEMU_GUI_SERIAL_LOG ?= target/run-gui-serial.log
QEMU_GPU_DEVICE ?= virtio-gpu-device,xres=3008,yres=1692
GDB ?= $(if $(filter aarch64,$(ARCH)),aarch64-none-elf-gdb,riscv64-elf-gdb)
ADDR2LINE ?= $(if $(filter aarch64,$(ARCH)),aarch64-none-elf-addr2line,riscv64-unknown-elf-addr2line)

export ARCH ACCEL PROFILE FS_IMAGE_SIZE_MIB AGENT_FS_IMAGE_SIZE_MIB
export QEMU_MEMORY AGENT_QEMU_MEMORY QEMU_GUI_DISPLAY QEMU_GUI_WINDOW_WIDTH
export QEMU_GUI_WINDOW_HEIGHT QEMU_GUI_SERIAL_LOG QEMU_GPU_DEVICE QEMU_SMP
export GDB ADDR2LINE

WORKFLOW := $(PYTHON) scripts/workflow.py

.PHONY: \
	build-kernel build-bootloader build-musl build-rootfs build-rust-std \
	prepare-rootfs reset-rootfs sync-userland prepare-agent-development \
	run-agent-development build-apk-apps regen-font regen-ui-font run run-gui \
	run-gdb clean clean-musl clean-busybox build verify verify-fast \
	verify-runtime verify-riscv64-secondary verify-unit \
	verify-architecture-benchmark verify-architecture-release \
	verify-runtime-gates verify-runtime-boot verify-runtime-audio \
	verify-runtime-frame-timing verify-runtime-musl verify-runtime-rust-std \
	verify-runtime-busybox verify-runtime-apk-apps verify-musl verify-rust-std \
	verify-busybox verify-apk-apps gdb addr2line

build-kernel build-bootloader build-musl build-rootfs build-rust-std \
prepare-rootfs reset-rootfs sync-userland prepare-agent-development \
run-agent-development build-apk-apps regen-font regen-ui-font run run-gui \
run-gdb clean clean-musl clean-busybox build verify verify-fast verify-runtime \
verify-riscv64-secondary verify-unit verify-architecture-benchmark \
verify-architecture-release verify-runtime-gates verify-runtime-boot \
verify-runtime-audio verify-runtime-frame-timing verify-runtime-musl \
verify-runtime-rust-std verify-runtime-busybox verify-runtime-apk-apps \
verify-musl verify-rust-std verify-busybox verify-apk-apps gdb addr2line:
	$(WORKFLOW) $@
