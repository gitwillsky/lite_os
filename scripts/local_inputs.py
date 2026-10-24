"""工程内持久输入的唯一目录 owner；均被 Git 忽略，不随 target/ 清理。

输入身份仍由各构建配方的版本、摘要与 manifest 裁决；本模块只定义存放位置。
"""
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LOCAL = ROOT / ".local"
INPUTS = LOCAL / "inputs"
KEYS = LOCAL / "keys"
TOOLS = LOCAL / "tools"
SPIKES = LOCAL / "spikes"
