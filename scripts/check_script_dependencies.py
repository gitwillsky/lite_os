#!/usr/bin/env python3
"""检查 Python 构建层不能反向依赖门禁，以及仓库内脚本的循环依赖。"""
from __future__ import annotations

import ast
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def command_module(name: str) -> bool:
    """门禁、诊断与 workflow 是最外层入口，允许调用生产构建层。"""
    return name == "workflow" or name.rsplit(".", 1)[-1].startswith(("verify_", "diagnose_", "check_"))


def check(root: Path) -> list[str]:
    """解析全部脚本（包括函数内和条件导入），返回带源码位置的依赖违规。"""
    sources = {
        ".".join(path.relative_to(root).with_suffix("").parts).removesuffix(".__init__"): path
        for path in root.rglob("*.py")
        if "tests" not in path.relative_to(root).parts
    }
    graph: dict[str, set[str]] = {}
    errors: list[str] = []
    for name, path in sources.items():
        dependencies: set[str] = set()
        for node in ast.walk(ast.parse(path.read_text(), filename=str(path))):
            if isinstance(node, ast.Import):
                imports = [alias.name for alias in node.names]
            elif isinstance(node, ast.ImportFrom):
                package = name.split(".") if path.name == "__init__.py" else name.split(".")[:-1]
                prefix = package[:len(package) - node.level + 1] if node.level else []
                module = ".".join((*prefix, *((node.module or "").split(".") if node.module else ())))
                imports = [module, *(f"{module}.{alias.name}" for alias in node.names)]
            else:
                continue
            for dependency in imports:
                if not command_module(name) and ((dependency in sources and command_module(dependency)) or dependency.split(".")[0] == "tests"):
                    errors.append(f"{path.relative_to(root)}:{node.lineno}: production build imports command {dependency}")
                if dependency in sources and dependency != name:
                    dependencies.add(dependency)
        graph[name] = dependencies

    finished: set[str] = set()
    active: list[str] = []

    def visit(name: str) -> None:
        if name in active:
            errors.append("script dependency cycle: " + " -> ".join((*active[active.index(name):], name)))
            return
        if name in finished:
            return
        active.append(name)
        for dependency in sorted(graph[name]):
            visit(dependency)
        active.pop()
        finished.add(name)

    for name in sorted(graph):
        visit(name)
    return errors


def main() -> int:
    errors = check(ROOT)
    for error in errors:
        print(error)
    if not errors:
        print("script dependency fence passed")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
