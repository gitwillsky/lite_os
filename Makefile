# ARCH 是唯一 compile-time backend selector；缺省选择 first-class AArch64。
ARCH ?= aarch64
# ACCEL 只决定 QEMU acceleration/CPU contract；HVF 失败必须显式报错，不能静默降级 TCG。
ACCEL ?= hvf
# release 是性能产品路径；debug 仅用于带 runtime assertion 的显式诊断构建。
PROFILE ?= release

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

export ARCH ACCEL
TARGET_QUERY = ARCH=$(ARCH) ACCEL=$(ACCEL) python3 scripts/build_target.py --field
KERNEL_TARGET := $(shell $(TARGET_QUERY) KERNEL_TARGET)
LINUX_TARGET := $(shell $(TARGET_QUERY) LINUX_TARGET)
QEMU := $(shell $(TARGET_QUERY) QEMU)
QEMU_CPU := $(shell $(TARGET_QUERY) QEMU_CPU)
QEMU_MACHINE := $(shell $(TARGET_QUERY) QEMU_MACHINE)
MUSL_LOADER := $(shell $(TARGET_QUERY) MUSL_LOADER)
ALPINE_ARCH := $(shell $(TARGET_QUERY) ALPINE_ARCH)
BOOTLOADER_REQUIRED := $(shell $(TARGET_QUERY) BOOTLOADER_REQUIRED)
KERNEL_BOOT_NAME := $(shell $(TARGET_QUERY) KERNEL_BOOT_NAME)

ifeq ($(PROFILE),release)
CARGO_PROFILE_ARG := --release
else
CARGO_PROFILE_ARG :=
endif

KERNEL_ELF := target/$(KERNEL_TARGET)/$(PROFILE)/kernel
KERNEL_BOOT_ARTIFACT := target/$(KERNEL_TARGET)/$(PROFILE)/$(KERNEL_BOOT_NAME)
ROOTFS_IMAGE := target/rootfs/$(ARCH).img
FS_IMAGE := fs-$(ARCH).img
APK_APPS_IMAGE := target/apk-apps/$(ARCH).img
# FS_IMAGE_SIZE_MIB 只控制可持续修改的开发实例；缺少扩容会让 GUI 内安装 Node.js 等应用时 ENOSPC。
FS_IMAGE_SIZE_MIB ?= 8192
# Agent 开发实例独立扩到 32 GiB；产品 rootfs/runtime gates 不继承开发工具容量。
AGENT_FS_IMAGE_SIZE_MIB ?= 32768

.PHONY: build-kernel build-bootloader build-musl build-rootfs build-rust-std prepare-rootfs reset-rootfs sync-userland prepare-agent-development run-agent-development build-apk-apps regen-font regen-ui-font run run-gui run-gdb clean clean-musl clean-busybox build verify verify-riscv64-secondary verify-unit verify-architecture-benchmark verify-architecture-release verify-runtime-gates verify-runtime-boot verify-runtime-audio verify-runtime-frame-timing verify-runtime-musl verify-runtime-rust-std verify-runtime-busybox verify-runtime-apk-apps verify-musl verify-rust-std verify-busybox verify-apk-apps gdb addr2line

QEMU_GUI_DISPLAY ?= cocoa,zoom-to-fit=off
QEMU_GPU_DEVICE ?= virtio-gpu-device,xres=3008,yres=1692
QEMU_GUI_SERIAL_LOG ?= target/run-gui-serial.log
QEMU_MEMORY ?= 2G
# Codex/Claude 与 Guest 内编译共享该开发 RAM；缺少独立值会把产品 2 GiB 门禁误改为重型配置。
AGENT_QEMU_MEMORY ?= 6G
QEMU_SMP ?= $(shell python3 scripts/host_topology.py)

ifeq ($(ARCH),aarch64)
QEMU_BOOT_ARGS :=
QEMU_AUDIO_HEADLESS_ARGS := -audiodev none,id=audio0 -device virtio-sound-device,audiodev=audio0,streams=1
QEMU_AUDIO_GUI_ARGS := -audiodev coreaudio,id=audio0,out.frequency=48000,out.channels=2 -device virtio-sound-device,audiodev=audio0,streams=1
GDB ?= aarch64-none-elf-gdb
GDB_ARCH := aarch64
ADDR2LINE ?= aarch64-none-elf-addr2line
else
QEMU_BOOT_ARGS := -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader
QEMU_AUDIO_HEADLESS_ARGS :=
QEMU_AUDIO_GUI_ARGS :=
GDB ?= riscv64-elf-gdb
GDB_ARCH := riscv:rv64
ADDR2LINE ?= riscv64-unknown-elf-addr2line
endif

build-kernel:
	cd kernel && cargo build --target $(KERNEL_TARGET) $(CARGO_PROFILE_ARG)
	python3 scripts/verify_artifacts.py --build-boot-artifact --profile $(PROFILE)

build-bootloader:
	@if [ "$(BOOTLOADER_REQUIRED)" = "0" ]; then exit 0; fi; \
	cd bootloader && cargo build --release && cd -

build-musl:
	python3 scripts/verify_musl.py --build-only

build-rootfs: build-kernel build-bootloader build-musl
	python3 scripts/verify_busybox.py --build-only --image $(ROOTFS_IMAGE)

build-rust-std: build-musl
	python3 scripts/verify_rust_std.py --build-only

# target/rootfs/<arch>.img 是可复现基线；fs-<arch>.img 是 guest 可持续修改的开发实例。
reset-rootfs: build-rootfs
	@temporary="$(FS_IMAGE).$$$$.tmp"; \
	trap 'rm -f "$$temporary"' 0 1 2 3 15; \
	cp "$(ROOTFS_IMAGE)" "$$temporary"; \
	python3 scripts/resize_ext2_image.py --image "$$temporary" --size-mib "$(FS_IMAGE_SIZE_MIB)"; \
	mv -f "$$temporary" "$(FS_IMAGE)"; \
	trap - 0 1 2 3 15

# 仅在开发镜像不存在时初始化；已有 target-specific 实例不以 mtime 与基线同步。
$(FS_IMAGE):
	$(MAKE) reset-rootfs

# QEMU 启动前离线扩容；只增长不缩容，因此保留已有开发数据。
prepare-rootfs: $(FS_IMAGE)
	python3 scripts/resize_ext2_image.py --image "$(FS_IMAGE)" --size-mib "$(FS_IMAGE_SIZE_MIB)"

# 图形用户态按内容增量替换；镜像内软件、项目和用户数据不属于该清单。
sync-userland: build-musl prepare-rootfs
	python3 scripts/sync_userland.py --image "$(FS_IMAGE)"

# Agent CLI 只进入持久 AArch64 开发实例；固定产品 rootfs 与跨架构 runtime gates 不安装这些工具。
prepare-agent-development:
	@if [ "$(ARCH)" != "aarch64" ]; then \
		echo "Agent development currently supports only ARCH=aarch64"; \
		exit 1; \
	fi
	$(MAKE) FS_IMAGE_SIZE_MIB=$(AGENT_FS_IMAGE_SIZE_MIB) build-kernel build-bootloader sync-userland
	python3 scripts/prepare_agent_development.py --image "$(FS_IMAGE)" --size-mib "$(AGENT_FS_IMAGE_SIZE_MIB)" --qemu-memory "$(AGENT_QEMU_MEMORY)"

run-agent-development: prepare-agent-development
	$(MAKE) FS_IMAGE_SIZE_MIB=$(AGENT_FS_IMAGE_SIZE_MIB) QEMU_MEMORY=$(AGENT_QEMU_MEMORY) run-gui

build-apk-apps: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_apk_apps.py --build-only --image $(ROOTFS_IMAGE) --output $(APK_APPS_IMAGE)

# 正常构建只消费 checked atlas；字体升级由显式目标完成，避免环境 FreeType 差异污染构建。
regen-font:
	target/fontenv/bin/python scripts/generate_terminal_font.py

# UI 比例字体 OpenType 子集由固定 venv 生成；正常构建只消费 checked 资产，不需要 fontTools。
regen-ui-font:
	target/fontenv/bin/python scripts/generate_ui_font.py

run: build-kernel build-bootloader sync-userland
	$(QEMU) \
	-machine $(QEMU_MACHINE) \
	-cpu $(QEMU_CPU) \
	-global virtio-mmio.force-legacy=false \
	-nographic \
	-m $(QEMU_MEMORY) \
	-smp $(QEMU_SMP) \
	-rtc base=utc \
	$(QEMU_BOOT_ARGS) -kernel $(KERNEL_BOOT_ARTIFACT) \
	-drive file=$(FS_IMAGE),if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-object rng-random,filename=/dev/urandom,id=rng0 \
	-device virtio-rng-device,rng=rng0 \
	-device $(QEMU_GPU_DEVICE) \
	$(QEMU_AUDIO_HEADLESS_ARGS) \
	-netdev user,id=net0 \
	-device virtio-net-device,netdev=net0

run-gui: build-kernel build-bootloader sync-userland
	@if [ "$$(uname -s)" = Darwin ]; then \
		/usr/bin/osascript scripts/activate_macos_process.applescript "$$$$" >/dev/null & \
	fi; \
	exec $(QEMU) \
	-machine $(QEMU_MACHINE) \
	-cpu $(QEMU_CPU) \
	-global virtio-mmio.force-legacy=false \
	-display $(QEMU_GUI_DISPLAY) \
	-serial file:$(QEMU_GUI_SERIAL_LOG) \
	-monitor none \
	-m $(QEMU_MEMORY) \
	-smp $(QEMU_SMP) \
	-rtc base=utc \
	$(QEMU_BOOT_ARGS) -kernel $(KERNEL_BOOT_ARTIFACT) \
	-drive file=$(FS_IMAGE),if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-object rng-random,filename=/dev/urandom,id=rng0 \
	-device virtio-rng-device,rng=rng0 \
	-device $(QEMU_GPU_DEVICE) \
	-device virtio-keyboard-device \
	-device virtio-tablet-device \
	$(QEMU_AUDIO_GUI_ARGS) \
	-netdev user,id=net0 \
	-device virtio-net-device,netdev=net0

run-gdb: build-kernel build-bootloader prepare-rootfs
	$(QEMU) -machine $(QEMU_MACHINE) -cpu $(QEMU_CPU) -global virtio-mmio.force-legacy=false -m $(QEMU_MEMORY) -smp $(QEMU_SMP) $(QEMU_BOOT_ARGS) -nographic -kernel $(KERNEL_BOOT_ARTIFACT) -drive file=$(FS_IMAGE),if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0 -object rng-random,filename=/dev/urandom,id=rng0 -device virtio-rng-device,rng=rng0 -device $(QEMU_GPU_DEVICE) -netdev user,id=net0 -device virtio-net-device,netdev=net0 -S -s

clean:
	cargo clean
	cd bootloader && cargo clean && cd -
	rm -f fs-aarch64.img fs-riscv64.img

clean-musl:
	rm -rf target/musl-runtime

clean-busybox:
	rm -rf target/busybox-runtime

build: build-kernel build-bootloader build-rootfs

verify:
	cargo fmt --all -- --check
	cargo clippy -p architecture-check -p architecture-bench -p kernel-unit -p scheduler-unit --all-targets -- -D warnings
	cargo clippy -p syscall-abi -p kernel --target $(KERNEL_TARGET) --bins --lib -- -D warnings
	@if [ "$(BOOTLOADER_REQUIRED)" = "0" ]; then exit 0; fi; \
	cd bootloader && cargo clippy --release -- -D warnings && cd -
	$(MAKE) verify-unit
	$(MAKE) verify-architecture-benchmark
	$(MAKE) verify-architecture-release
	$(MAKE) build
	cargo run --quiet -p architecture-check
	python3 scripts/verify_artifacts.py
	$(MAKE) -j4 verify-runtime-gates
	@if [ "$(ARCH)" = "aarch64" ]; then \
		$(MAKE) verify-riscv64-secondary; \
	fi
	git diff --check

# ARM64 是完整提交门禁 owner；RISC-V 作为保留 backend 只执行 compile/static/boot smoke。
# 缺少此门禁会让通用 façade 在 ARM64 默认路径通过后静默破坏 RISC-V backend。
verify-riscv64-secondary:
	$(MAKE) ARCH=riscv64 ACCEL=tcg PROFILE=release build-kernel build-bootloader build-rootfs
	cargo clippy -p syscall-abi -p kernel --target riscv64gc-unknown-none-elf --bins --lib -- -D warnings
	cd bootloader && cargo clippy --release -- -D warnings
	$(MAKE) ARCH=riscv64 ACCEL=tcg PROFILE=release verify-architecture-release
	ARCH=riscv64 ACCEL=tcg python3 scripts/verify_artifacts.py
	ARCH=riscv64 ACCEL=tcg python3 scripts/verify_boot.py --image target/rootfs/riscv64.img
	ARCH=riscv64 ACCEL=tcg python3 scripts/verify_rust_std.py --image target/rootfs/riscv64.img

verify-unit:
	cargo test -p architecture-check -p kernel-unit -p scheduler-unit -p syscall-abi
	user_test_assets="$$(python3 scripts/verify_busybox.py --build-ui-assets-only)" && \
		LITE_UI_TEST_ASSETS="$$user_test_assets" cargo test --manifest-path user/Cargo.toml --workspace

verify-architecture-benchmark:
	cargo run --quiet --release -p architecture-bench

verify-architecture-release:
	cd kernel && cargo build --target $(KERNEL_TARGET) --release
	python3 scripts/verify_architecture_release.py
	python3 scripts/check_trap_cost.py

verify-runtime-gates:
	$(MAKE) verify-runtime-boot
	$(MAKE) verify-runtime-musl
	$(MAKE) verify-runtime-rust-std
	$(MAKE) verify-runtime-busybox
	$(MAKE) verify-runtime-apk-apps
	$(MAKE) verify-runtime-audio
	$(MAKE) verify-runtime-frame-timing

verify-runtime-boot:
	python3 scripts/verify_boot.py --image $(ROOTFS_IMAGE)

verify-runtime-audio:
	@if [ "$(ARCH)-$(ACCEL)" = "aarch64-hvf" ]; then \
		python3 scripts/verify_audio.py --image $(ROOTFS_IMAGE); \
	else \
		echo "audio runtime gate skipped: $(ARCH)/$(ACCEL) retains compile/static/boot coverage only"; \
	fi

verify-runtime-frame-timing:
	python3 scripts/verify_frame_timing.py --image $(ROOTFS_IMAGE)

verify-runtime-musl:
	python3 scripts/verify_musl.py

verify-runtime-rust-std:
	python3 scripts/verify_rust_std.py --image $(ROOTFS_IMAGE)

verify-runtime-busybox:
	python3 scripts/verify_busybox.py --image $(ROOTFS_IMAGE)

verify-runtime-apk-apps:
	python3 scripts/verify_apk_apps.py --image $(ROOTFS_IMAGE)

verify-musl: build-kernel build-bootloader
	python3 scripts/verify_musl.py

verify-rust-std: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_rust_std.py --image $(ROOTFS_IMAGE)

verify-busybox: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_busybox.py --image $(ROOTFS_IMAGE)

verify-apk-apps: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_apk_apps.py --image $(ROOTFS_IMAGE)

gdb:
	$(GDB) -ex 'file $(KERNEL_ELF)' -ex 'target remote :1234' -ex 'set arch $(GDB_ARCH)'

addr2line:
	@$(ADDR2LINE) -e $(KERNEL_ELF) -f -p $(ADDR)
