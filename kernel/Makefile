run:
	cargo build
	qemu-system-riscv64 -machine virt -bios ../target/riscv64gc-unknown-none-elf/debug/bootloader -nographic -kernel ../target/riscv64gc-unknown-none-elf/debug/kernel

clean:
	cargo clean

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/lite_os' -ex 'target remote :1234' -ex 'set arch riscv:rv64'