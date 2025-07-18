#!/bin/bash
# Auto-generated symbol update script

KERNEL_PATH="$1"
SYMBOLS_FILE="$2"

if [ ! -f "$KERNEL_PATH" ]; then
    echo "Kernel binary not found: $KERNEL_PATH"
    exit 1
fi

echo "Extracting symbols from: $KERNEL_PATH"

# 使用rust-nm提取符号
TEMP_SYMBOLS=$(mktemp)
rust-nm --print-size --radix=x --defined-only "$KERNEL_PATH" | grep -v "\.L" | grep " [Tt] " > "$TEMP_SYMBOLS"

# 生成新的symbols.rs
cat > "$SYMBOLS_FILE" << 'EOF'
// Auto-generated symbol table from binary analysis
pub fn populate_symbol_table(table: &mut SymbolTable) {
    // Symbols extracted from kernel binary
EOF

# 添加核心符号
rust-nm "$KERNEL_PATH" | grep -E "^[0-9a-f]+ [A-Za-z] (_start|strampoline|stext|etext|trap|panic|main)" | while read addr type name; do
    addr_hex="0x$addr"
    echo "    table.add_symbol(String::from(\"$name\"), $addr_hex, 64);" >> "$SYMBOLS_FILE"
done

# 添加所有源码内的kernel模块函数
rust-nm --print-size --radix=x --defined-only --demangle "$KERNEL_PATH" | grep -E " [Tt] " | \
    grep "kernel::" | \
    # grep -v "closure" | grep -v "drop_in_place" | grep -v "fmt::" | \
    while read addr size type name; do
    addr_hex="0x$addr"
    size_dec=$((0x$size))
    # 保留完整的模块路径和函数名
    clean_name=$(echo "$name" | sed 's/kernel:://' | sed 's/<.*>//' | sed 's/::h[0-9a-f]*$//' | cut -d'(' -f1)
    if [ ${#clean_name} -gt 3 ] && [ ${#clean_name} -lt 100 ]; then
        echo "    table.add_symbol(String::from(\"$clean_name\"), $addr_hex, $size_dec);" >> "$SYMBOLS_FILE"
    fi
done

# 添加系统调用函数（如果有的话）
rust-nm --print-size --radix=x --defined-only --demangle "$KERNEL_PATH" | grep -E " [Tt] " | \
    grep "sys_" | \
    while read addr size type name; do
    addr_hex="0x$addr"
    size_dec=$((0x$size))
    clean_name=$(echo "$name" | sed 's/<.*>//' | cut -d'(' -f1)
    echo "    table.add_symbol(String::from(\"$clean_name\"), $addr_hex, $size_dec);" >> "$SYMBOLS_FILE"
done

# 添加重要的非mangled符号
rust-nm --defined-only "$KERNEL_PATH" | grep -E " [Tt] " | \
    grep -v "_ZN" | grep -v "\.L" | \
    head -20 | while read addr type name; do
    addr_hex="0x$addr"
    echo "    table.add_symbol(String::from(\"$name\"), $addr_hex, 64);" >> "$SYMBOLS_FILE"
done

echo "}" >> "$SYMBOLS_FILE"

rm -f "$TEMP_SYMBOLS"
echo "Symbols updated successfully"
