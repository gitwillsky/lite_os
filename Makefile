ELFS := $(patsubst user/src/bin/%.rs, target/riscv64gc-unknown-none-elf/release/%, $(wildcard user/src/bin/*.rs))
OBJCOPY := rust-objcopy --binary-architecture=riscv64

build-user:
	cd user && cargo build --release && cd -
	@$(foreach elf, $(ELFS), $(OBJCOPY) $(elf) --strip-all -O binary $(patsubst target/riscv64gc-unknown-none-elf/release/%, target/riscv64gc-unknown-none-elf/release/%.bin, $(elf));)

build-kernel:
	cd kernel && cargo build  && cd -

build-bootloader:
	cd bootloader && cargo build --release && cd -

run-with-timeout: build-kernel
	sleep 20 && killall qemu-system-riscv64 & \
	qemu-system-riscv64 \
	-smp 4 \
	-machine virt \
	-nographic \
	-bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader \
	-kernel target/riscv64gc-unknown-none-elf/debug/kernel \
	-drive file=fs.img,if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-device virtio-rng-device \
	-device virtio-gpu-device \
	-device virtio-mouse-device \
	-rtc base=localtime \
	-device virtio-net-device,netdev=net0 \
	-netdev user,id=net0,hostfwd=tcp::5555-:5555

run: build-kernel
	qemu-system-riscv64 \
	-smp 4 \
	-machine virt \
	-nographic \
	-rtc base=localtime \
	-bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader \
	-kernel target/riscv64gc-unknown-none-elf/debug/kernel \
	-drive file=fs.img,if=none,format=raw,id=x0 \
	-device virtio-blk-device,drive=x0 \
	-device virtio-rng-device \
	-device virtio-gpu-device \
	-device virtio-mouse-device \
	-device virtio-net-device,netdev=net0 \
	-netdev user,id=net0,hostfwd=tcp::5555-:5555

run-gdb: build-kernel
	qemu-system-riscv64 -machine virt -bios bootloader/target/riscv64gc-unknown-none-elf/release/bootloader -nographic -kernel target/riscv64gc-unknown-none-elf/debug/kernel -drive file=fs.img,if=none,format=raw,id=x0 -device virtio-blk-device,drive=x0 -S -s

clean:
	cargo clean

build: build-user build-kernel build-bootloader create-fs

gdb:
	riscv64-elf-gdb -ex 'file target/riscv64gc-unknown-none-elf/debug/kernel' -ex 'target remote :1234' -ex 'set arch riscv:rv64'

create-fs:
	python3 create_fs.py create

addr2line:
	@riscv64-unknown-elf-addr2line -e target/riscv64gc-unknown-none-elf/debug/kernel -f -p $(ADDR)