ROOTFS_IMAGE := target/rootfs.img

.PHONY: build-kernel build-bootloader build-musl build-rootfs reset-rootfs build-apk-apps run run-gui run-gdb clean clean-musl clean-busybox build verify verify-runtime-gates verify-runtime-boot verify-runtime-musl verify-runtime-busybox verify-runtime-apk-apps verify-musl verify-busybox verify-apk-apps gdb addr2line

build-kernel:
	cd kernel && cargo build  && cd -

build-bootloader:
	cd bootloader && cargo build --release && cd -

build-musl:
	python3 scripts/verify_musl.py --build-only

build-rootfs: build-musl
	python3 scripts/verify_busybox.py --build-only --image $(ROOTFS_IMAGE)

# target/rootfs.img 是可复现基线；fs.img 是 guest 可持续修改的开发实例。
reset-rootfs: build-rootfs
	@temporary="fs.img.$$$$.tmp"; \
	trap 'rm -f "$$temporary"' 0 1 2 3 15; \
	cp "$(ROOTFS_IMAGE)" "$$temporary"; \
	mv -f "$$temporary" fs.img; \
	trap - 0 1 2 3 15

# 仅在开发镜像不存在时初始化；已有 fs.img 不以 mtime 与基线同步。
fs.img:
	$(MAKE) reset-rootfs

build-apk-apps: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_apk_apps.py --build-only --image $(ROOTFS_IMAGE) --output target/apk-apps.img

run: build-kernel build-bootloader fs.img
	qemu-system-riscv64 \
	-machine virt \
	-global virtio-mmio.force-legacy=false \
	-nographic \
	-smp 8 \
	-rtc base=localtime \
	-bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader \
	-kernel target/riscv64gc-unknown-none-elf/debug/kernel \
	-drive file=fs.img,if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-object rng-random,filename=/dev/urandom,id=rng0 \
	-device virtio-rng-device,rng=rng0 \
	-device virtio-gpu-device \
	-netdev user,id=net0 \
	-device virtio-net-device,netdev=net0

run-gui: build-kernel build-bootloader fs.img
	qemu-system-riscv64 \
	-machine virt \
	-global virtio-mmio.force-legacy=false \
	-display default \
	-serial mon:stdio \
	-smp 8 \
	-rtc base=localtime \
	-bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader \
	-kernel target/riscv64gc-unknown-none-elf/debug/kernel \
	-drive file=fs.img,if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-object rng-random,filename=/dev/urandom,id=rng0 \
	-device virtio-rng-device,rng=rng0 \
	-device virtio-gpu-device \
	-netdev user,id=net0 \
	-device virtio-net-device,netdev=net0

run-gdb: build-kernel build-bootloader fs.img
	qemu-system-riscv64 -machine virt -global virtio-mmio.force-legacy=false -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0 -object rng-random,filename=/dev/urandom,id=rng0 -device virtio-rng-device,rng=rng0 -device virtio-gpu-device -netdev user,id=net0 -device virtio-net-device,netdev=net0 -S -s

clean:
	cargo clean
	cd bootloader && cargo clean && cd -
	rm -f fs.img

clean-musl:
	rm -rf target/musl-runtime

clean-busybox:
	rm -rf target/busybox-runtime

build: build-kernel build-bootloader build-rootfs

verify:
	cargo fmt --all -- --check
	cargo clippy -p architecture-check -- -D warnings
	cargo clippy -p syscall-abi -p kernel --target riscv64gc-unknown-none-elf --bins --lib -- -D warnings
	cd bootloader && cargo clippy --release -- -D warnings && cd -
	$(MAKE) build
	cargo run --quiet -p architecture-check
	python3 scripts/verify_artifacts.py
	$(MAKE) -j4 verify-runtime-gates
	git diff --check

verify-runtime-gates: verify-runtime-boot verify-runtime-musl verify-runtime-busybox verify-runtime-apk-apps

verify-runtime-boot:
	python3 scripts/verify_boot.py --image $(ROOTFS_IMAGE)

verify-runtime-musl:
	python3 scripts/verify_musl.py

verify-runtime-busybox:
	python3 scripts/verify_busybox.py --image $(ROOTFS_IMAGE)

verify-runtime-apk-apps:
	python3 scripts/verify_apk_apps.py --image $(ROOTFS_IMAGE)

verify-musl: build-kernel build-bootloader
	python3 scripts/verify_musl.py

verify-busybox: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_busybox.py --image $(ROOTFS_IMAGE)

verify-apk-apps: build-kernel build-bootloader build-rootfs
	python3 scripts/verify_apk_apps.py --image $(ROOTFS_IMAGE)

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/kernel' -ex 'target remote :1234' -ex 'set arch riscv:rv64'

addr2line:
	@riscv64-unknown-elf-addr2line -e target/riscv64gc-unknown-none-elf/debug/kernel -f -p $(ADDR)
