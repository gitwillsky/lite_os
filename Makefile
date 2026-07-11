build-user:
	cd user && cargo build --release && cd -

build-kernel:
	cd kernel && cargo build  && cd -

build-bootloader:
	cd bootloader && cargo build --release && cd -

run: build-bootloader build-kernel build-user create-fs
	qemu-system-riscv64 \
	-machine virt \
	-nographic \
	-smp 8 \
	-rtc base=localtime \
	-bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader \
	-kernel target/riscv64gc-unknown-none-elf/debug/kernel \
	-drive file=fs.img,if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0

run-gdb: build-bootloader build-kernel build-user create-fs
	qemu-system-riscv64 -machine virt -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0 -S -s

clean:
	cargo clean

build: build-user build-kernel build-bootloader create-fs

verify:
	cargo fmt --all -- --check
	cargo check --workspace
	$(MAKE) build-user
	$(MAKE) build-kernel
	$(MAKE) build-bootloader
	python3 scripts/architecture_check.py
	git diff --check

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/kernel' -ex 'target remote :1234' -ex 'set arch riscv:rv64'

create-fs:
	python3 create_fs.py create

addr2line:
	@riscv64-unknown-elf-addr2line -e target/riscv64gc-unknown-none-elf/debug/kernel -f -p $(ADDR)
