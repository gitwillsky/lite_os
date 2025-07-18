#!/bin/bash

# Build script for WASM test programs
set -e

echo "Building Rust WASM Test Programs for LiteOS"
echo "============================================"

# 检查 Rust 和 wasm32-wasi target
if ! rustc --version &> /dev/null; then
    echo "Error: Rust compiler not found!"
    echo "Please install Rust: https://rustup.rs/"
    exit 1
fi

echo "Rust version: $(rustc --version)"

# 检查 wasm32-wasip1 target
if ! rustup target list --installed | grep -q "wasm32-wasip1"; then
    echo "Installing wasm32-wasip1 target..."
    rustup target add wasm32-wasip1
fi

# 创建输出目录
mkdir -p wasm_output

echo ""
echo "Building WASM programs..."

# 编译所有测试程序
programs=("hello_wasm" "wasi_test" "math_test" "file_test")

for program in "${programs[@]}"; do
    echo "Building $program..."
    cargo build --release --bin "$program"
    
    # 复制生成的 WASM 文件到输出目录
    if [ -f "target/wasm32-wasip1/release/$program.wasm" ]; then
        cp "target/wasm32-wasip1/release/$program.wasm" "wasm_output/"
        echo "✓ $program.wasm created"
    else
        echo "✗ Failed to build $program.wasm"
        exit 1
    fi
done

echo ""
echo "Build completed successfully!"
echo ""
echo "Generated WASM files:"
ls -la wasm_output/*.wasm

echo ""
echo "File sizes:"
for file in wasm_output/*.wasm; do
    size=$(wc -c < "$file")
    echo "  $(basename "$file"): $size bytes"
done

echo ""
echo "To test these WASM files:"
echo "  1. Copy them to the LiteOS filesystem"
echo "  2. Run: ./wasm_runtime <filename>.wasm"
echo ""
echo "Files ready for copying to fs.img!"