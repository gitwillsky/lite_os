build:
	cd bootloader && cargo build --release && cd -
	cd kernel && cargo build  && cd -

run: build
	qemu-system-riscv64 -machine virt -bios target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel

run-gdb: build
	qemu-system-riscv64 -machine virt -bios target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -S -s

clean:
	cargo clean

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/kernel' -ex 'target remote :1234' -ex 'set arch riscv:rv64'


ELFS := $(patsubst user/src/bin/%.rs, target/riscv64gc-unknown-none-elf/release/%, $(wildcard user/src/bin/*.rs))
OBJCOPY := rust-objcopy --binary-architecture=riscv64

build-user:
	cd user && cargo build --release && cd -
	@$(foreach elf, $(ELFS), $(OBJCOPY) $(elf) --strip-all -O binary $(patsubst target/riscv64gc-unknown-none-elf/release/%, target/riscv64gc-unknown-none-elf/release/%.bin, $(elf));)
