.PHONY: build-kernel build-bootloader build-musl build-rootfs run run-gdb clean clean-musl clean-busybox build verify verify-musl verify-busybox gdb addr2line

build-kernel:
	cd kernel && cargo build  && cd -

build-bootloader:
	cd bootloader && cargo build --release && cd -

build-musl:
	python3 scripts/verify_musl.py --build-only

build-rootfs: build-kernel build-bootloader build-musl
	python3 scripts/verify_busybox.py --build-only --image fs.img

run: build
	qemu-system-riscv64 \
	-machine virt \
	-nographic \
	-smp 8 \
	-rtc base=localtime \
	-bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader \
	-kernel target/riscv64gc-unknown-none-elf/debug/kernel \
	-drive file=fs.img,if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-object rng-random,filename=/dev/urandom,id=rng0 \
	-device virtio-rng-device,rng=rng0 \
	-netdev user,id=net0 \
	-device virtio-net-device,netdev=net0

run-gdb: build
	qemu-system-riscv64 -machine virt -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0 -object rng-random,filename=/dev/urandom,id=rng0 -device virtio-rng-device,rng=rng0 -netdev user,id=net0 -device virtio-net-device,netdev=net0 -S -s

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
	python3 scripts/verify_boot.py
	python3 scripts/verify_musl.py
	python3 scripts/verify_busybox.py
	git diff --check

verify-musl: build-kernel build-bootloader
	python3 scripts/verify_musl.py

verify-busybox: build-kernel build-bootloader build-musl
	python3 scripts/verify_busybox.py

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/kernel' -ex 'target remote :1234' -ex 'set arch riscv:rv64'

addr2line:
	@riscv64-unknown-elf-addr2line -e target/riscv64gc-unknown-none-elf/debug/kernel -f -p $(ADDR)
