build: build-user
	cd bootloader && cargo build --release && cd -
	cd kernel && cargo build  && cd -

run: build
	qemu-system-riscv64 -machine virt -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0

run-gdb: build
	qemu-system-riscv64 -machine virt -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0 -S -s

clean:
	cargo clean

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/kernel' -ex 'target remote :1234' -ex 'set arch riscv:rv64'

create-fs:
	dd if=/dev/zero of=fs.img bs=1M count=128
	echo "Creating FAT32 filesystem..."
	# 在macOS上，我们可以使用newfs_msdos
	newfs_msdos -F 32 fs.img || echo "FAT32 filesystem creation failed, but file created"

ELFS := $(patsubst user/src/bin/%.rs, target/riscv64gc-unknown-none-elf/release/%, $(wildcard user/src/bin/*.rs))
OBJCOPY := rust-objcopy --binary-architecture=riscv64

build-user:
	cd user && cargo build --release && cd -
	@$(foreach elf, $(ELFS), $(OBJCOPY) $(elf) --strip-all -O binary $(patsubst target/riscv64gc-unknown-none-elf/release/%, target/riscv64gc-unknown-none-elf/release/%.bin, $(elf));)
